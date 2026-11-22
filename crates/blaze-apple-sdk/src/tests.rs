//! The resolution as a pure function of what the edge read: `SDKROOT`'s value, xcrun's answer
//! or failure, and the settings and build files of the one folder chosen.

use std::{format, println};

use super::{Build, Found, NotSdk, SdkId, SdkRefused, Unread, Version, XcrunFailed, identity_of, resolve, settings_version, system_version_build};

const SETTINGS_27_2: &str = r#"{"CanonicalName":"macosx27.2","VersionMap":{"Version":"1.0","macOS_iOSMac":{"27.2":"27.2"}},"Version":"27.2"}"#;

/// A 27.2 SDK's `SystemVersion.plist`, trimmed of keys not read.
const PLIST_26B118: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>ProductBuildVersion</key>\n\t<string>26B118</string>\n\t<key>ProductVersion</key>\n\t<string>27.2</string>\n</dict>\n</plist>\n";

/// A machine whose settings file is `SETTINGS_27_2` at `/sdk/a`, `/sdk/b` and `/sdk/nobuild`,
/// `{}` at `/sdk/empty`, and missing elsewhere.
fn settings(folder: &str) -> Result<&'static str, Unread<'static>> {
    match folder {
        "/sdk/a" | "/sdk/b" | "/sdk/nobuild" => Ok(SETTINGS_27_2),
        "/sdk/empty" => Ok("{}"),
        _ => Err(Unread { why: "No such file or directory (os error 2)" }),
    }
}

/// Its build file: `PLIST_26B118` at `/sdk/a` and `/sdk/b`, missing elsewhere.
fn system_version(folder: &str) -> Result<&'static str, Unread<'static>> {
    match folder {
        "/sdk/a" | "/sdk/b" => Ok(PLIST_26B118),
        _ => Err(Unread { why: "No such file or directory (os error 2)" }),
    }
}

fn never() -> Result<&'static str, XcrunFailed<'static>> {
    panic!("xcrun was asked although SDKROOT is set")
}

fn id_27_2() -> SdkId {
    SdkId { version: Version::new(27, 2, 0).expect("in range"), build: Build::parse("26B118").expect("a build") }
}

#[test]
fn sdkroot_set_to_an_sdk_folder_is_taken_and_xcrun_is_not_asked() {
    let sdk = resolve(Some("/sdk/a"), never, settings, system_version).expect("SDKROOT names an SDK");
    assert_eq!((sdk.folder(), sdk.found(), sdk.version_text()), ("/sdk/a", Found::SdkRoot, "27.2"));
    assert_eq!(sdk.identity(), id_27_2());
    assert_eq!(sdk.version(), Version::new(27, 2, 0).expect("in range"));
    println!("SDKROOT taken");
}

#[test]
fn sdkroot_set_to_no_sdk_folder_is_refused_naming_the_value() {
    let refused = resolve(Some("/nowhere"), never, settings, system_version).expect_err("no SDK there");
    assert_eq!(refused, SdkRefused::SdkRoot { value: "/nowhere", why: NotSdk::Unread(Unread { why: "No such file or directory (os error 2)" }) });
    let words = format!("{refused}");
    assert!(words.contains("SDKROOT=/nowhere") && words.contains("SDKSettings.json"), "{words}");
    // An SDK's name is xcrun's form, and a folder with no version is no SDK.
    assert_eq!(resolve(Some("macosx"), never, settings, system_version), Err(SdkRefused::SdkRoot { value: "macosx", why: NotSdk::NotAbsolute }));
    assert_eq!(resolve(Some("/sdk/empty"), never, settings, |_| Ok(PLIST_26B118)), Err(SdkRefused::SdkRoot { value: "/sdk/empty", why: NotSdk::NoVersion }));
    // A folder with a version and no build file is no SDK either, named by the file.
    let refused = resolve(Some("/sdk/nobuild"), never, settings, system_version).expect_err("no build");
    assert!(matches!(refused, SdkRefused::SdkRoot { value: "/sdk/nobuild", why: NotSdk::BuildUnread(_) }), "{refused:?}");
    assert!(format!("{refused}").contains("SystemVersion.plist"), "{refused}");
    println!("SDKROOT refused by its value");
}

#[test]
fn sdkroot_unset_takes_the_folder_xcrun_printed() {
    for unset in [None, Some("")] {
        let sdk = resolve(unset, || Ok("/sdk/b\n"), settings, system_version).expect("xcrun names an SDK");
        assert_eq!((sdk.folder(), sdk.found()), ("/sdk/b", Found::Xcrun));
        assert_eq!(sdk.identity(), id_27_2());
    }
    let refused = resolve(None, || Ok("/gone\n"), settings, system_version).expect_err("xcrun printed no SDK folder");
    assert!(matches!(refused, SdkRefused::Xcrun { printed: "/gone", why: NotSdk::Unread(_) }), "{refused:?}");
    println!("xcrun's folder taken");
}

#[test]
fn sdkroot_unset_and_xcrun_failing_names_both_and_how_to_set_it() {
    let failures = [
        XcrunFailed::Exited { status: 1, said: "xcrun: error: unable to find utility" },
        XcrunFailed::Expired { after: super::XCRUN_DEADLINE },
        XcrunFailed::Failed { why: "No such file or directory (os error 2)" },
        XcrunFailed::NotMacos,
    ];
    for xcrun in failures {
        let refused = resolve(None, || Err(xcrun), |_| panic!("no folder to read"), |_| panic!("no folder to read")).expect_err("no SDK");
        assert_eq!(refused, SdkRefused::Neither { xcrun });
        let words = format!("{refused}");
        assert!(words.contains("SDKROOT is not set") && words.contains("xcrun --sdk macosx --show-sdk-path"), "{words}");
        assert!(words.contains("export SDKROOT="), "the refusal says how to set SDKROOT: {words}");
    }
    println!("both attempts named");
}

/// Two builds of one version are two identities, and their keys differ; one build in two
/// folders is one. Prints "builds keyed". Control: drop the build from `SdkId::key` and the
/// second assertion fails with "a beta and the release of 27.2 share a key".
#[test]
fn two_builds_of_one_version_are_two_identities() {
    let beta = PLIST_26B118.replace("26B118", "26B5044k");
    let (_, release) = identity_of(SETTINGS_27_2, PLIST_26B118).expect("an identity");
    let (_, early) = identity_of(SETTINGS_27_2, &beta).expect("an identity");
    assert_eq!(release, id_27_2());
    assert_eq!(release.version, early.version);
    assert_ne!(release.key(), early.key(), "a beta and the release of 27.2 share a key");
    assert_eq!(format!("{release}"), "27.2 (26B118)");
    // The build is read only from its own key, and refused when it is no build.
    assert_eq!(system_version_build("<key>ProductVersion</key><string>27.2</string>"), None);
    assert_eq!(system_version_build("<key>ProductBuildVersion</key>\n <string>26 B</string>"), None);
    assert_eq!(system_version_build("<key>ProductBuildVersion</key><string>12345678901234567</string>"), None);
    assert_eq!(identity_of(SETTINGS_27_2, "<plist/>"), Err(NotSdk::NoBuild));
    println!("builds keyed");
}

#[test]
fn versions_and_the_sdks_settings_are_read_as_rustc_reads_them() {
    let nibbles = |text: &str| Version::parse(text).map(Version::nibbles);
    assert_eq!(nibbles("11"), Some(0x000b_0000));
    assert_eq!(nibbles("10.15.7"), Some(0x000a_0f07));
    for bad in ["", "11.", "11.0.0.0", "x", "11.256", "+11", "11 .0", "65536"] {
        assert_eq!(nibbles(bad), None, "{bad}");
    }
    let json = r#"{ "Nested": {"Version": "1"}, "List": ["Version", {"Version": "2"}], "Name": "Version", "Version" : "15.2" }"#;
    assert_eq!(settings_version(json), Some(("15.2", Version::new(15, 2, 0).expect("in range"))));
    assert_eq!(settings_version(r#"{"Version": 15}"#), None);
    assert_eq!(settings_version(r#"{"Other": "a\"Version\": \"9\""}"#), None);
    // A `VersionMap`'s nested `Version` is never the SDK's.
    assert_eq!(settings_version(SETTINGS_27_2).map(|it| it.0), Some("27.2"));
    assert_eq!(format!("{}", Version::new(10, 15, 7).expect("in range")), "10.15.7");
    assert_eq!(format!("{}", Version::new(27, 0, 0).expect("in range")), "27.0");
    println!("versions read");
}
