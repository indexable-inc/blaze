//! The native setting's producer (D112, brief N1) and what it reaches on the real path.
//!
//! What is right: a command with the pinned toolchain unpacked on its shelf and an SDK whose
//! `SDKSettings.json` gives a `Version` has a native setting, and a script placed native
//! ([`crate::native_unit::placed`]) then gets as far as looking for its host binary
//! (`script_unit::module_of_native`): with none in the store, the refusal is `NoModule`, not
//! `NoNativeSetting`. A missing shelf is refused naming the shelf's path. Nothing here runs a
//! script, clang or the sandbox: the fake home holds empty folders and one settings file.
//!
//! Expected from `lanecheck.sh blaze-proc-macro test native_setting`:
//! `test blaze-proc-macro: exit 0; 3 passed, 0 failed`; the last test prints
//! "native: with a setting NoModule for zstd-sys 1.0.0, without NoNativeSetting".

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use blaze_build_script::{key::CMode, toolchain::Mode};
use blaze_manifest::lock::LockId;
use blaze_own::{AVec, Alloc};
use blaze_unit_graph::{Resolver, Text, Unit, UnitDep, UnitGraph, UnitId, UnitKind};
use edition::PackageEdition;

use crate::{
    native_setting::{NATIVE_RUN_DEADLINE, NATIVE_WORK_DIR, NativeSettingError, OwnedNativeSetting, SHELF_DIR, command_native_setting},
    native_unit::{NativeSetting, NativeUnitError},
    script::ScriptRuntime,
    script_sandbox::{ScriptRoot, Tree},
    script_unit::{Setting, UnitScriptError, outputs},
    spawn_route::SpawnSetting,
};

const HOST: &str = "aarch64-apple-darwin";
/// The aarch64-apple-darwin row's sha256 in pins/clang.json.
const PIN_HEX: &str = "5d3179db39adeca5b17a34d82708fe976bc60e055b05fe557878fb3fb64d39ef";

/// A fresh folder under the system's temporary one, by this process and `name`.
fn folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("blaze-native-setting-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a fresh test folder");
    dir
}

/// A home with the pin's shelf (when `shelf`) and an SDK folder giving `Version` 15.2.
fn fake_home(name: &str, shelf: bool) -> (std::string::String, std::string::String) {
    let home = folder(name);
    if shelf {
        // nm-row: every member the setting looks for (toolchain::SHELF_MEMBERS), empty.
        let dir = home.join(SHELF_DIR).join(PIN_HEX);
        for member in blaze_build_script::toolchain::SHELF_MEMBERS {
            let path = dir.join(member);
            if member == "lib/clang" {
                std::fs::create_dir_all(&path).expect("the resource folder");
            } else {
                std::fs::create_dir_all(path.parent().expect("bin/ or lib/")).expect("the shelf folder");
                std::fs::write(&path, b"").expect("a shelf member");
            }
        }
    }
    let sdk = home.join("MacOSX.sdk");
    std::fs::create_dir_all(&sdk).expect("the SDK folder");
    std::fs::write(sdk.join("SDKSettings.json"), r#"{"CanonicalName":"macosx15.2","Version":"15.2"}"#).expect("SDKSettings.json");
    (home.to_str().expect("a UTF-8 temp folder").to_owned(), sdk.to_str().expect("a UTF-8 temp folder").to_owned())
}

fn produce<'a>(home: &str, sdk: &str, alloc: Alloc<'a>) -> Result<OwnedNativeSetting<'a>, NativeSettingError<'a>> {
    command_native_setting(home, sdk, "test", HOST, alloc)
}

/// The command's jobserver under the setting's work folder, two workers.
fn jobserver(owned: &OwnedNativeSetting<'_>) -> crate::jobserver::Jobserver {
    let opts = crate::jobserver::JobserverOpts { work: owned.work.as_str(), workers: core::num::NonZeroU32::new(2).expect("2"), id: std::process::id() };
    crate::jobserver::Jobserver::new(opts).unwrap_or_else(|e| panic!("{e}"))
}

/// The producer names the pin's shelf, the SDK's version and the command's own work folder.
/// A producer that ignored the pin table (any other folder name) or the SDK's settings fails.
#[test]
fn the_native_setting_names_the_pins_shelf_the_sdks_version_and_the_commands_work_folder() {
    let alloc = blaze_own::testing::heap().alloc();
    let (home, sdk) = fake_home("produced", true);
    let owned = produce(&home, &sdk, alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(owned.shelf.as_str().rsplit('/').next(), Some(PIN_HEX));
    assert_eq!(owned.sdk_version.as_str(), "15.2");
    assert!(owned.sdk.as_str().ends_with("/MacOSX.sdk"), "{}", owned.sdk.as_str());
    assert!(owned.work.as_str().ends_with(&format!("{NATIVE_WORK_DIR}/test")), "{}", owned.work.as_str());
    assert!(std::fs::metadata(owned.work.as_str()).map(|m| m.is_dir()).unwrap_or(false), "the work folder is made");
    let jobs = jobserver(&owned);
    let setting: NativeSetting<'_> = owned.setting(&jobs);
    assert_eq!(setting.deadline, NATIVE_RUN_DEADLINE);
    assert!(setting.jobs.fifo().starts_with(owned.work.as_str()), "the fifo is the command's own: {}", setting.jobs.fifo());
    jobs.end().unwrap_or_else(|e| panic!("{e}"));
    let _ = std::fs::remove_dir_all(&home);
}

/// No shelf: refused naming the shelf's path, never another toolchain (rule 3).
#[test]
fn a_missing_shelf_is_refused_naming_its_path() {
    let alloc = blaze_own::testing::heap().alloc();
    let (home, sdk) = fake_home("no-shelf", false);
    match produce(&home, &sdk, alloc) {
        Err(NativeSettingError::NoShelf { path, .. }) => {
            assert_eq!(path.as_str(), format!("{home}/{SHELF_DIR}/{PIN_HEX}"));
        }
        Err(other) => panic!("not the missing shelf: {other}"),
        Ok(owned) => panic!("a setting with no shelf: {}", owned.shelf.as_str()),
    }
    let _ = std::fs::remove_dir_all(&home);
}

fn t(s: &str) -> Text<'static> {
    Text::from_str_in(s, blaze_own::testing::heap().alloc())
}

fn unit(name: &str, kind: UnitKind<'static>, deps: &[u32], build_script: Option<u32>) -> Unit<'static> {
    let alloc = blaze_own::testing::heap().alloc();
    let mut d = AVec::new_in(alloc);
    for u in deps {
        d.push(UnitDep { name: t("dep"), unit: UnitId(*u) });
    }
    Unit {
        package: LockId(0),
        name: t(name),
        version: t("1.0.0"),
        kind,
        for_host: true,
        edition: PackageEdition::of_manifest("2021").expect("an edition"),
        crate_name: t(name),
        root: t("build.rs"),
        features: AVec::new_in(alloc),
        cfg: AVec::new_in(alloc),
        deps: d,
        build_script: build_script.map(UnitId),
        links: None,
    }
}

/// A `build` command's setting over `store` with `native` (no spawns, no try-compile answers).
fn setting<'s>(store: &'s Path, native: Option<NativeSetting<'s>>) -> Setting<'s> {
    Setting {
        store,
        target: HOST,
        host: HOST,
        table: Arc::new(Vec::new()),
        probes: None,
        c_probes: None,
        c_mode: CMode::Built,
        mode: Mode::Build,
        native,
        spawn_setting: Arc::new(SpawnSetting::off()),
        files_digest: None,
        deps_digest: None,
    }
}

/// zstd-sys's script (its closure holds `cc`) is placed native for `build`; with the
/// producer's setting the real path (`script_unit::outputs`) reaches the host binary's lookup
/// and, the store holding none, refuses with `NoModule`; with no setting, `NoNativeSetting`. A
/// setting that never reached the native path (left `None` by the command) fails the first
/// match.
#[test]
fn a_native_placed_script_with_a_setting_reaches_its_binary_lookup() {
    let alloc = blaze_own::testing::heap().alloc();
    let (home, sdk) = fake_home("reaches", true);
    let owned = produce(&home, &sdk, alloc).unwrap_or_else(|e| panic!("{e}"));

    let mut units = AVec::new_in(alloc);
    units.push(unit("zstd-sys", UnitKind::BuildScript, &[1], None)); // 0
    units.push(unit("cc", UnitKind::Lib, &[], None)); // 1
    units.push(unit("zstd-sys", UnitKind::Lib, &[], Some(0))); // 2
    let graph = UnitGraph { units, resolver: Resolver::V3, meta: AVec::new_in(alloc), left_out: AVec::new_in(alloc) };

    let store = folder("reaches-store");
    let runtime = ScriptRuntime::new(&store).unwrap_or_else(|e| panic!("{e}"));
    let setting_with = |native| setting(&store, native);
    let run = |setting: &Setting<'_>| {
        let tree = Tree::Mem { files: Arc::new(Vec::new()), submodules: Arc::from([]), git: None };
        outputs(&runtime, setting, &graph, UnitId(0), tree, ScriptRoot::package(), &[], alloc)
    };

    let jobs = jobserver(&owned);
    let with = match run(&setting_with(Some(owned.setting(&jobs)))) {
        Err(UnitScriptError::Native(NativeUnitError::NoModule { package, .. })) => package,
        Err(other) => panic!("with a native setting, not the binary's lookup: {other}"),
        Ok(_) => panic!("a run with no host binary in the store"),
    };
    assert_eq!(with, "zstd-sys 1.0.0");
    match run(&setting_with(None)) {
        Err(UnitScriptError::Native(NativeUnitError::NoNativeSetting { .. })) => {}
        Err(other) => panic!("with no native setting, not NoNativeSetting: {other}"),
        Ok(_) => panic!("a native run with no setting"),
    }
    println!("native: with a setting NoModule for {with}, without NoNativeSetting");
    jobs.end().unwrap_or_else(|e| panic!("{e}"));
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&store);
}

/// Linux ARM64 uses `Sysroot::LinuxHost`: accepts a Linux shelf (no `lib/libclang.dylib` or
/// `bin/ld64.lld`) and keys the sysroot by `<sdk>/include/features.h`, refusing when absent.
#[test]
fn linux_host_native_setting_accepts_linux_shelf_and_features_h() {
    use blaze_build_script::toolchain::{LINUX_HOST_SYSROOT_MARKER, SHELF_MEMBERS_LINUX, Sysroot};
    const LINUX_HOST: &str = "aarch64-unknown-linux-gnu";
    const LINUX_PIN_HEX: &str = "9b27cf263d711aa6ebfd4045ab241d947bd09b3cd19ffa5728f48a80c9eb3a91";
    let alloc = blaze_own::testing::heap().alloc();
    let home = folder("linux-host");
    let shelf = home.join(SHELF_DIR).join(LINUX_PIN_HEX);
    for member in SHELF_MEMBERS_LINUX {
        let path = shelf.join(member);
        if member == "lib/clang" {
            fs::create_dir_all(&path).expect("resource dir");
        } else {
            fs::create_dir_all(path.parent().expect("parent")).expect("bin");
            fs::write(&path, b"").expect("member");
        }
    }
    let sdk = home.join("usr");
    fs::create_dir_all(sdk.join("include")).expect("include");
    let home_s = home.to_str().expect("utf8");
    let sdk_s = sdk.to_str().expect("utf8");

    // Missing `include/features.h` is refused as `NoSdk`.
    let missing = command_native_setting(home_s, sdk_s, "test", LINUX_HOST, alloc).expect_err("missing features.h");
    assert!(matches!(missing, NativeSettingError::NoSdk { .. }), "{missing}");

    // With `include/features.h`, setting succeeds with `Sysroot::LinuxHost` and a 64-hex digest.
    fs::write(sdk.join(LINUX_HOST_SYSROOT_MARKER), b"#define __GLIBC__ 2\n").expect("features.h");
    let owned = command_native_setting(home_s, sdk_s, "test", LINUX_HOST, alloc).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(owned.sysroot, Sysroot::LinuxHost);
    assert_eq!(owned.sdk_extra, None);
    assert_eq!(owned.shelf.as_str().rsplit('/').next(), Some(LINUX_PIN_HEX));
    assert_eq!(owned.sdk_version.len(), 64);

    // NixOS split kernel headers: `<sdk>/include/linux` symlinks to a separate store root.
    // `sdk_extra` records that root for Landlock, and `sdk_version` commits to every header file
    // byte under both `<sdk>/include` and `<sdk_extra>/include` (e.g. changing `asm/errno.h`
    // while keeping `features.h` and `linux/version.h` identical must move `sdk_version`), while
    // remaining path-independent across two identical copies at different directories.
    #[cfg(unix)]
    {
        let headers = home.join("linux-headers-7.2");
        fs::create_dir_all(headers.join("include/linux")).expect("linux headers dir");
        fs::create_dir_all(headers.join("include/asm")).expect("asm headers dir");
        fs::write(headers.join("include/linux/version.h"), b"#define LINUX_VERSION_CODE 1\n").expect("version.h");
        fs::write(headers.join("include/asm/errno.h"), b"#define EINVAL 22\n").expect("asm/errno.h");
        std::os::unix::fs::symlink(headers.join("include/linux"), sdk.join("include/linux")).expect("symlink linux");
        std::os::unix::fs::symlink(headers.join("include/asm"), sdk.join("include/asm")).expect("symlink asm");

        let split_a = command_native_setting(home_s, sdk_s, "test", LINUX_HOST, alloc).unwrap_or_else(|e| panic!("{e}"));
        let expected_extra = fs::canonicalize(&headers).expect("canon headers");
        assert_eq!(split_a.sdk_extra.as_ref().map(|s| s.as_str()), expected_extra.to_str());
        assert_eq!(split_a.sdk_version.len(), 64);
        assert_ne!(split_a.sdk_version.as_str(), owned.sdk_version.as_str());

        // Identical header bytes in a different directory produce the exact same path-independent `sdk_version`.
        let sdk_copy = home.join("glibc-copy");
        let headers_copy = home.join("linux-headers-copy");
        fs::create_dir_all(sdk_copy.join("include")).expect("copy inc");
        fs::create_dir_all(headers_copy.join("include/linux")).expect("copy linux");
        fs::create_dir_all(headers_copy.join("include/asm")).expect("copy asm");
        fs::write(sdk_copy.join(LINUX_HOST_SYSROOT_MARKER), b"#define __GLIBC__ 2\n").expect("copy features.h");
        fs::write(headers_copy.join("include/linux/version.h"), b"#define LINUX_VERSION_CODE 1\n").expect("copy version.h");
        fs::write(headers_copy.join("include/asm/errno.h"), b"#define EINVAL 22\n").expect("copy asm/errno.h");
        std::os::unix::fs::symlink(headers_copy.join("include/linux"), sdk_copy.join("include/linux")).expect("copy symlink linux");
        std::os::unix::fs::symlink(headers_copy.join("include/asm"), sdk_copy.join("include/asm")).expect("copy symlink asm");
        let split_copy = command_native_setting(home_s, sdk_copy.to_str().expect("utf8"), "test", LINUX_HOST, alloc)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(split_a.sdk_version.as_str(), split_copy.sdk_version.as_str());

        // Changing an arbitrary header (`asm/errno.h`) while keeping `features.h` and `linux/version.h`
        // unchanged MUST invalidate `sdk_version`.
        fs::write(headers.join("include/asm/errno.h"), b"#define EINVAL 22\n#define EAGAIN 11\n").expect("asm/errno.h v2");
        let split_b = command_native_setting(home_s, sdk_s, "test", LINUX_HOST, alloc).unwrap_or_else(|e| panic!("{e}"));
        assert_ne!(split_a.sdk_version.as_str(), split_b.sdk_version.as_str());

        // Retargeting a directory symlink (`include/asm` from `arch-a` to `arch-b` while BOTH
        // target trees remain present on disk) MUST move `sdk_version`.
        let retarget_sdk = home.join("glibc-retarget");
        fs::create_dir_all(retarget_sdk.join("include/arch-a")).expect("arch-a");
        fs::create_dir_all(retarget_sdk.join("include/arch-b")).expect("arch-b");
        fs::write(retarget_sdk.join(LINUX_HOST_SYSROOT_MARKER), b"#define __GLIBC__ 2\n").expect("retarget features.h");
        fs::write(retarget_sdk.join("include/arch-a/errno.h"), b"#define EINVAL 22\n").expect("arch-a errno.h");
        fs::write(retarget_sdk.join("include/arch-b/errno.h"), b"#define EINVAL 99\n").expect("arch-b errno.h");
        std::os::unix::fs::symlink(retarget_sdk.join("include/arch-a"), retarget_sdk.join("include/asm")).expect("asm -> arch-a");
        let retarget_a = command_native_setting(home_s, retarget_sdk.to_str().expect("utf8"), "test", LINUX_HOST, alloc)
            .unwrap_or_else(|e| panic!("{e}"));
        fs::remove_file(retarget_sdk.join("include/asm")).expect("rm asm link");
        std::os::unix::fs::symlink(retarget_sdk.join("include/arch-b"), retarget_sdk.join("include/asm")).expect("asm -> arch-b");
        let retarget_b = command_native_setting(home_s, retarget_sdk.to_str().expect("utf8"), "test", LINUX_HOST, alloc)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_ne!(
            retarget_a.sdk_version.as_str(),
            retarget_b.sdk_version.as_str(),
            "retargeting directory symlink include/asm must move sdk_version even when both target dirs stay on disk"
        );

        // Split libc runtime `sdk_lib` is canonicalized, stored, and hashed into `sdk_version`:
        // changing any link input (e.g. `crt1.o`) invalidates `sdk_version`.
        let libc_lib = home.join("glibc-2.44/lib");
        fs::create_dir_all(&libc_lib).expect("glibc lib");
        fs::write(libc_lib.join("crt1.o"), b"crt-v1").expect("crt1.o");
        fs::write(libc_lib.join("libc.so"), b"GROUP ( libc.so.6 )\n").expect("libc.so");
        let libc_lib_s = libc_lib.to_str().expect("utf8");
        let with_lib_a = crate::native_setting::command_native_setting_with_lib(
            home_s,
            sdk_s,
            Some(libc_lib_s),
            "test",
            LINUX_HOST,
            alloc,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let expected_lib = fs::canonicalize(&libc_lib).expect("canon lib");
        assert_eq!(with_lib_a.sdk_lib.as_ref().map(|s| s.as_str()), expected_lib.to_str());
        assert_ne!(with_lib_a.sdk_version.as_str(), split_b.sdk_version.as_str());

        fs::write(libc_lib.join("crt1.o"), b"crt-v2").expect("crt1.o v2");
        let with_lib_b = crate::native_setting::command_native_setting_with_lib(
            home_s,
            sdk_s,
            Some(libc_lib_s),
            "test",
            LINUX_HOST,
            alloc,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_ne!(with_lib_a.sdk_version.as_str(), with_lib_b.sdk_version.as_str());

        // Negative control: a symlink loop on `<sdk>/include/linux` propagates `NoSdk` naming
        // `include/linux` instead of silently dropping `sdk_extra`.
        let broken_sdk = home.join("glibc-broken");
        fs::create_dir_all(broken_sdk.join("include")).expect("broken inc");
        fs::write(broken_sdk.join(LINUX_HOST_SYSROOT_MARKER), b"#define __GLIBC__ 2\n").expect("broken features.h");
        std::os::unix::fs::symlink(broken_sdk.join("include/linux"), broken_sdk.join("include/linux")).expect("loop symlink");
        let loop_err = command_native_setting(home_s, broken_sdk.to_str().expect("utf8"), "test", LINUX_HOST, alloc)
            .expect_err("symlink loop on include/linux must fail with NoSdk");
        match loop_err {
            NativeSettingError::NoSdk { path, .. } => {
                assert!(path.as_str().ends_with("/include/linux"), "expected include/linux path, got {}", path.as_str());
            }
            other => panic!("expected NoSdk, got {other}"),
        }

        // Negative control: symlinks to non-regular targets (`/dev/zero`, `/dev/null`, and a real
        // Unix FIFO) inside a non-FHS sysroot must be rejected immediately as `NoSdk` without
        // opening or blocking.
        let fifo_dir = home.join("fifo-target");
        fs::create_dir_all(&fifo_dir).expect("fifo dir");
        let fifo_path = fifo_dir.join("hang.fifo");
        let fifo_c = std::ffi::CString::new(fifo_path.to_str().expect("utf8 fifo")).expect("cstr");
        // SAFETY: `fifo_c` is a valid NUL-terminated path pointing into our temp test dir.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0, "mkfifo failed");

        for (tag, target) in [
            ("dev-null", std::path::Path::new("/dev/null")),
            ("dev-zero", std::path::Path::new("/dev/zero")),
            ("fifo", fifo_path.as_path()),
        ] {
            let hostile_sdk = home.join(format!("glibc-hostile-{tag}"));
            fs::create_dir_all(hostile_sdk.join("include")).expect("hostile inc");
            fs::write(hostile_sdk.join(LINUX_HOST_SYSROOT_MARKER), b"#define __GLIBC__ 2\n").expect("hostile features.h");
            std::os::unix::fs::symlink(target, hostile_sdk.join("include/hostile.h")).expect("hostile symlink");
            let err = command_native_setting(home_s, hostile_sdk.to_str().expect("utf8"), "test", LINUX_HOST, alloc)
                .expect_err("non-regular symlink target must be refused immediately");
            match err {
                NativeSettingError::NoSdk { path, .. } => {
                    assert!(path.as_str().ends_with("/include/hostile.h"), "expected hostile.h path for {tag}, got {}", path.as_str());
                }
                other => panic!("expected NoSdk for {tag}, got {other}"),
            }
        }
    }
    let _ = fs::remove_dir_all(&home);
}
