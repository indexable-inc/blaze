//! The macOS SDK, resolved once at the command's start (rule 75): the edge half of
//! blaze-apple-sdk's [`blaze_apple_sdk::resolve`], which decides. Here `SDKROOT` is the value
//! `main` read from the command's environment; only when it is unset is xcrun run, with the
//! command's `PATH`, `DEVELOPER_DIR` and `HOME`, by [`XCRUN_DEADLINE`] (rule 18), killed past
//! it; and the one folder chosen has its `SDKSettings.json` read. What comes out is a value
//! the request carries ([`crate::fetch::FetchEnv::sdk`]): the native build-script runner takes
//! its folder and the scripts' keys its identity (scripts.rs); nothing deeper reads the machine
//! for it. A refusal fails only what needs the SDK (a script placed native), by name.

use blaze_apple_sdk::{SETTINGS, Sdk, SdkRefused, Unread, XcrunFailed};
use bstd::Cx;
use bstd::mem::Frame;

/// What the edge read of the command's environment for xcrun: `PATH` (where `xcrun` is
/// found), `DEVELOPER_DIR` (which developer folder xcrun asks) and `HOME`.
pub(crate) struct XcrunEnv<'e> {
    pub(crate) path: Option<&'e str>,
    pub(crate) developer_dir: Option<&'e str>,
    pub(crate) home: Option<&'e str>,
}

/// The command's macOS SDK: `sdkroot`, else what xcrun says, its settings read from the
/// folder chosen; the words of a failure held in `frame`.
pub(crate) fn resolve<'f>(cx: &Cx, sdkroot: Option<&'f str>, env: &XcrunEnv<'_>, frame: &'f Frame) -> Result<Sdk<'f>, SdkRefused<'f>> {
    let cx = cx.span("sdk: the macOS SDK folder");
    blaze_apple_sdk::resolve(sdkroot, || xcrun(&cx, env, frame), |folder| settings(folder, frame))
}

/// `words` held in `frame`.
fn held<'f>(frame: &'f Frame, words: core::fmt::Arguments<'_>) -> &'f str {
    let text = bstd::mem::format_in!(frame, "{words}");
    crate::work::held_str(text.as_str(), frame)
}

/// The text of `<folder>/SDKSettings.json`, held in `frame`.
fn settings<'f>(folder: &str, frame: &'f Frame) -> Result<&'f str, Unread<'f>> {
    let path = held(frame, format_args!("{folder}/{SETTINGS}"));
    match std::fs::read(path) {
        Ok(bytes) => match core::str::from_utf8(&bytes) {
            Ok(text) => Ok(crate::work::held_str(text, frame)),
            Err(_) => Err(Unread { why: "it is not UTF-8" }),
        },
        Err(error) => Err(Unread { why: held(frame, format_args!("{error}")) }),
    }
}

/// What `xcrun --sdk macosx --show-sdk-path` printed, run by its deadline: started linked in a
/// group of its own, so a kill at the deadline ends all it started.
#[cfg(target_os = "macos")]
fn xcrun<'f>(cx: &Cx, env: &XcrunEnv<'_>, frame: &'f Frame) -> Result<&'f str, XcrunFailed<'f>> {
    use std::io::Read as _;

    use blaze_apple_sdk::{XCRUN, XCRUN_ARGS, XCRUN_DEADLINE};
    use blaze_core::linked::{Ended, Link, start_linked};
    use blaze_core::wait::Deadline;

    let _cx = cx.span("sdk: xcrun --sdk macosx --show-sdk-path");
    let failed = |words: core::fmt::Arguments<'_>| XcrunFailed::Failed { why: held(frame, words) };
    // Its whole environment, nothing else of this process's: the variables xcrun reads.
    let mut vars = std::vec::Vec::new_in(std::alloc::Global);
    for (name, value) in [("PATH", env.path), ("DEVELOPER_DIR", env.developer_dir), ("HOME", env.home)] {
        if let Some(value) = value {
            vars.push((std::ffi::OsString::from(name), std::ffi::OsString::from(value)));
        }
    }
    let command = blaze_core::Command {
        program: std::path::PathBuf::from(XCRUN),
        args: XCRUN_ARGS.iter().map(std::ffi::OsString::from).collect(),
        env: vars,
        dir: std::path::PathBuf::from("/"),
        stdin: std::vec::Vec::new_in(std::alloc::Global),
        sandbox: blaze_core::Sandbox::Off,
        memory: None,
    };
    let mut child = start_linked(&command, &Link { inherit: &[], own_group: true }).map_err(|error| failed(format_args!("{error}")))?;
    let ended = child.wait_until(Deadline::at(XCRUN_DEADLINE)).map_err(|error| failed(format_args!("{error}")))?;
    let (out, err) = child.take_outputs().ok_or_else(|| failed(format_args!("its output was taken before it was read")))?;
    let read = |fd: std::os::fd::OwnedFd| {
        let mut bytes = std::vec::Vec::new_in(std::alloc::Global);
        std::fs::File::from(fd).read_to_end(&mut bytes).map(|_| bytes)
    };
    match ended {
        Ended::Deadline => {
            child.kill_group().map_err(|error| failed(format_args!("{error}")))?;
            Err(XcrunFailed::Expired { after: XCRUN_DEADLINE })
        }
        Ended::Signaled(signal) => Err(XcrunFailed::Signaled { signal }),
        Ended::Exited(0) => {
            let bytes = read(out).map_err(|error| failed(format_args!("reading its output: {error}")))?;
            core::str::from_utf8(&bytes).map(|text| crate::work::held_str(text, frame)).map_err(|_| XcrunFailed::NotText)
        }
        Ended::Exited(status) => {
            let bytes = read(err).map_err(|error| failed(format_args!("reading its error stream: {error}")))?;
            Err(XcrunFailed::Exited { status, said: held(frame, format_args!("{}", std::string::String::from_utf8_lossy(&bytes).trim())) })
        }
    }
}

/// No xcrun off macOS: `SDKROOT` alone names an SDK there.
#[cfg(not(target_os = "macos"))]
fn xcrun<'f>(_cx: &Cx, _env: &XcrunEnv<'_>, _frame: &'f Frame) -> Result<&'f str, XcrunFailed<'f>> {
    Err(XcrunFailed::NotMacos)
}

/// A command's SDK in a test that runs no native script: none, said so.
#[cfg(test)]
pub(crate) const NONE_IN_TESTS: Result<Sdk<'static>, SdkRefused<'static>> =
    Err(SdkRefused::Neither { xcrun: XcrunFailed::Failed { why: "a test gives no macOS SDK" } });
