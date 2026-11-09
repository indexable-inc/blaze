//! The program `blaze` (also installed as `blaze-engine`, and run by cargo as `cargo-blaze`):
//! what runs before blaze's cargo, then blaze's cargo, the client of the daemon
//! ([`blaze_cargo::cli`]), which is also the daemon (`blaze serve`).
//!
//! Before it, in this order: the sandbox launcher (`blaze sandbox ...`, which forks, so it comes
//! before any thread); blaze started as a build script's `rustc` or `rustdoc`; the
//! release of blaze the workspace pins, run in this one's place when it is not this one
//! (`pin.rs`, `versions.rs`), or outside a pinned workspace the default `blaze update`
//! installed (`update.rs`); the lines about blaze's own updates; and the commands of the pins
//! and the updates themselves.

use std::{ffi::OsString, path::Path, process::ExitCode};

use crate::{as_rustc, pin, update, versions};

/// The program `blaze`, whose `src/main.rs` and `src/bin/blaze.rs` are only this.
pub fn main() -> ExitCode {
    // C's and C++'s allocations (LLVM's) are blaze's allocator's, counted to the running task's
    // tag as Rust's are (`malloc.rs`); a process where they are not stops here, saying why.
    #[cfg(feature = "native")]
    if let Err(why) = crate::malloc::check() {
        eprintln!("error: {why}");
        return ExitCode::FAILURE;
    }
    // A bug blaze reports and goes on past (`stdx::assert::reported`) is said by the daemon in
    // `blaze status` and the profile; it doesn't change how this process exits, as it does a
    // test's.
    stdx::assert::reported::keep_exit_status();
    if std::env::args_os().nth(1).is_some_and(|it| it == blaze_cargo::sandbox::COMMAND) {
        blaze_cargo::sandbox::command(std::env::args_os().skip(2));
    }
    blaze_cargo::release::set(blaze_cargo::release::Release {
        version: pin::OURS,
        line: env!("BLAZE_VERSION_LINE"),
    });
    // What the daemon runs off its loop to keep blaze's update state fresh (`update.rs`).
    blaze_cargo::release::set_updates(update::tick);
    // What every stored template is keyed by: the code generator's code, which build.rs worked
    // out, never this executable's bytes (`hir_ty_codegen::v2::code`).
    if let Err(why) = hir_ty_codegen::v2::CodegenCode::from_hex(env!("BLAZE_CODEGEN_CODE"))
        .and_then(hir_ty_codegen::v2::set_code)
    {
        eprintln!("error: {why}");
        return ExitCode::FAILURE;
    }
    // What images, stored goals and bodies and kept expansions are keyed by: the front end's
    // code, worked out in the same walk (`blaze_cargo::serve::front_end_code`).
    if let Err(why) = blaze_cargo::serve::FrontEndCode::from_hex(env!("BLAZE_FRONT_END_CODE"))
        .and_then(blaze_cargo::serve::set_front_end_code)
    {
        eprintln!("error: {why}");
        return ExitCode::FAILURE;
    }
    // What unit records and the standard library's shared directory are keyed by, with the two
    // above: the code of what makes a record, worked out in the same walk
    // (`blaze_cargo::serve::record_makers`), never this executable's bytes.
    if let Err(why) = blaze_cargo::serve::RecordCode::from_hex(env!("BLAZE_RECORD_CODE"))
        .and_then(blaze_cargo::serve::set_record_code)
    {
        eprintln!("error: {why}");
        return ExitCode::FAILURE;
    }
    if as_rustc::is_me() {
        as_rustc::command();
    }
    if as_rustc::rustdoc::is_me() {
        as_rustc::rustdoc::command();
    }
    // `--ignore-pin` is the launcher's alone: blaze's cargo never sees it.
    let (args, ignore_pin) = pin::take_ignore_pin(std::env::args_os());
    let words = blaze_cargo::cli::words(args.iter().cloned());
    let asks_version = matches!(words.as_slice(), [it] if it == "--version" || it == "-V");
    match pinned_elsewhere(&words, ignore_pin) {
        Ok(Ran::Ended(ended)) => return ended,
        Ok(Ran::Here { pin: pinned, manifest }) => {
            if says_updates(&words) {
                update::say(pin::OURS, pinned.as_policy(), manifest.as_deref());
            }
        }
        // `--version` answers in any workspace: which blaze this is, and why the one pinned
        // can't run here.
        Err(why) if asks_version => {
            println!("{}", blaze_cargo::version());
            println!("this blaze does not run here: {why}");
            return ExitCode::SUCCESS;
        }
        Err(why) => return failed(&why),
    }
    let first = words.first().and_then(|it| it.to_str());
    let rest = || words.iter().skip(1).map(|it| it.to_string_lossy().into_owned());
    let done = match first {
        Some("pin") => versions::pin(&rest().collect::<Vec<_>>()),
        Some("version-fetch") => versions::command(rest()).map(|()| 0),
        Some("version-resolve") => versions::resolve_command(rest()).map(|()| 0),
        Some("update") => update::command(rest()),
        Some("--version" | "-V") if words.len() == 1 => {
            println!("{}", blaze_cargo::version());
            println!("{}", versions::why_this_one(ignore_pin));
            Ok(0)
        }
        _ => {
            #[cfg(feature = "native")]
            if let Err(error) = crate::huge::huge_pages_for_mimalloc() {
                return failed(&format!("{error:#}"));
            }
            // `IX_PROF=<dir>`: this process (the client, or the daemon it starts) is sampled
            // until it ends and its profile written to `<dir>`; with `<dir>` a target
            // directory's `layout`, the next build of blaze there lays its code out by it
            // (docs/v2/plan/design/code-layout.md). Unset, nothing runs.
            let _profiled = match ix_prof::start_from_env() {
                Ok(guard) => guard,
                Err(error) => return failed(&error.to_string()),
            };
            return blaze_cargo::cli::main(args.into_iter());
        }
    };
    match done {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => failed(&format!("{error:#}")),
    }
}

/// The commands this blaze runs itself, wherever it is run: a pin's and an update's own, and
/// the daemon, which the client that starts it already chose.
const OWN: &[&str] = &["pin", "version-fetch", "version-resolve", "update", "serve"];

/// Whether `words` get the lines about blaze's updates (`update::say`): not the commands of
/// [`OWN`], and not in a blaze a launcher ran, which said them ([`update::launched`]).
fn says_updates(words: &[OsString]) -> bool {
    let first = words.first().and_then(|it| it.to_str());
    !first.is_some_and(|it| OWN.contains(&it)) && !update::launched()
}

/// What a workspace pins, for the lines about blaze's updates.
enum Pinned {
    None,
    Version(String),
    Channel,
    Ignored,
}

impl Pinned {
    fn as_policy(&self) -> update::Pin<'_> {
        match self {
            Pinned::None => update::Pin::None,
            Pinned::Version(version) => update::Pin::Version(version),
            Pinned::Channel => update::Pin::Channel,
            Pinned::Ignored => update::Pin::Ignored,
        }
    }

    /// What `named` (a version or a channel) pins.
    fn of(named: &str) -> Pinned {
        if pin::is_channel(named) { Pinned::Channel } else { Pinned::Version(named.to_owned()) }
    }
}

/// How [`pinned_elsewhere`] ended.
enum Ran {
    /// Another blaze ran the command, which ended so.
    Ended(ExitCode),
    /// This one runs it, under `pin`, in the workspace of `manifest` when there is one.
    Here { pin: Pinned, manifest: Option<std::path::PathBuf> },
}

/// Runs the release of blaze the workspace pins with `words`, when it pins one and that is not
/// this one, fetching it first when it is not there: [`Ran::Ended`] of how it ended. Outside a
/// pinned workspace, the same for the default `blaze update` installed ([`run_default`]).
/// [`Ran::Here`] when this one is to run: the pin names it, [`pin::IGNORE_PIN`] (`given` here,
/// or to a blaze above this one for this workspace: [`pin::ignores`]), or it is a build of the
/// source tree in blaze's tree; such a build anywhere else hands off as a release does, saying
/// so on stderr ([`pin::choose`]). `Err` is why the pinned one can't run, and no other runs in
/// its place. `blaze pin` always runs this one: it pins this one. Before another blaze runs,
/// the lines about blaze's updates are said here, for the release that runs.
fn pinned_elsewhere(words: &[OsString], given: bool) -> Result<Ran, String> {
    let text: Vec<String> = words.iter().map(|it| it.to_string_lossy().into_owned()).collect();
    if text.first().is_some_and(|it| OWN.contains(&it.as_str())) {
        return Ok(Ran::Here { pin: Pinned::None, manifest: None });
    }
    let cwd = std::env::current_dir()
        .map_err(|error| pin::PinError::NoWorkingDirectory(error).to_string())?;
    let Some(manifest) =
        pin::root_manifest(&pin::start(&text, &cwd)).map_err(|why| why.to_string())?
    else {
        return run_default(words, None);
    };
    let Some(named) = pin::pinned(&manifest).map_err(|why| why.to_string())? else {
        return run_default(words, Some(manifest));
    };
    let shown = manifest.display();
    let ignore = pin::ignores(given, &manifest);
    let for_tree = match pin::choose(pin::OURS, &manifest, &named, ignore)
        .map_err(|why| why.to_string())?
    {
        // `--ignore-pin` given here, for a workspace no blaze above this one was told it
        // for: this blaze runs again with `BLAZE_IGNORE_PIN_FOR` naming the manifest, so a
        // blaze a test or build script runs in this workspace (`$CARGO`) runs itself too,
        // and one run in another workspace is held to that one's pin. Run again, not set
        // here: setting a variable in this process is unsafe, and the word is rare.
        pin::Choice::Ignored
            if given
                && !pin::names(std::env::var_os(pin::IGNORE_PIN_FOR).as_deref(), &manifest) =>
        {
            let this = std::env::current_exe().map_err(|error| {
                    format!("`{}` runs this blaze again for {shown}, and where it is can't be told: {error}", pin::IGNORE_PIN)
                })?;
            let mut command = std::process::Command::new(&this);
            command.args(words).env(pin::IGNORE_PIN_FOR, &manifest);
            return Ok(Ran::Ended(ended(&mut command, &this)));
        }
        pin::Choice::Pinned => {
            return Ok(Ran::Here { pin: Pinned::of(&named), manifest: Some(manifest) });
        }
        pin::Choice::Ignored | pin::Choice::OwnTree => {
            return Ok(Ran::Here { pin: Pinned::Ignored, manifest: Some(manifest) });
        }
        pin::Choice::Release => false,
        pin::Choice::ReleaseForTree => true,
    };
    let ignoring = format!("`blaze {} <command>` runs this one", pin::IGNORE_PIN);
    let wanted = if pin::is_channel(&named) {
        channel_version(&named).map_err(|why| {
            format!("{shown} pins blaze's {named} channel, and {why}; no other blaze builds in its place ({ignoring})")
        })?
    } else {
        named.clone()
    };
    if wanted == pin::OURS {
        return Ok(Ran::Here { pin: Pinned::of(&named), manifest: Some(manifest) });
    }
    // Run by a launcher for this same manifest as the pinned release, and not it: the `blaze`
    // kept for that release is another. Only then: a blaze found on PATH by a process under the
    // pinned one (a build script, a test, a program `blaze run` runs) inherits
    // `BLAZE_PINNED_BY` too, and runs the pinned release again, which runs itself.
    if pin::names(std::env::var_os(pin::PINNED_BY).as_deref(), &manifest) && is_kept_for(&wanted) {
        let this = std::env::current_exe().unwrap_or_default();
        return Err(format!(
            "{shown} pins blaze {wanted}, and {}, which a blaze ran for it (`{}` names {shown}), is blaze {}: removing {} has it fetched again",
            this.display(),
            pin::PINNED_BY,
            pin::OURS,
            this.parent().unwrap_or(Path::new("")).display()
        ));
    }
    // A build of the tree runs the pinned release as a release does, and says so: what runs is
    // not the blaze that was built.
    if for_tree {
        eprintln!(
            "blaze: blaze {}, a build of blaze's source tree, runs blaze {wanted} here, as {shown} pins {}; {ignoring}",
            pin::OURS,
            pin::pinned_text(&named)
        );
    }
    let pinned = versions::fetch(&wanted).map_err(|why| {
        format!(
            "{shown} pins blaze {wanted}, which could not be fetched ({why:#}); no other blaze builds in its place ({ignoring})"
        )
    })?;
    // A release from before pins says only its version.
    if matches!(text.as_slice(), [it] if it == "--version" || it == "-V") {
        println!("blaze {} runs blaze {wanted} here, as {shown} pins it:", pin::OURS);
    }
    if says_updates(words) {
        update::say(&wanted, Pinned::of(&named).as_policy(), Some(&manifest));
    }
    let mut command = std::process::Command::new(&pinned);
    command.args(words).env(pin::PINNED_BY, &manifest);
    Ok(Ran::Ended(ended(&mut command, &pinned)))
}

/// Whether this process is the `blaze` kept for the release `version` (`pin::installed`): a
/// launcher that ran it as that release, and finds it another, is refused rather than run it
/// again. The paths are compared resolved where they can be.
fn is_kept_for(version: &str) -> bool {
    let (Ok(this), Some(kept)) = (std::env::current_exe(), pin::installed(version)) else {
        return false;
    };
    let resolved = |it: &Path| std::fs::canonicalize(it).unwrap_or_else(|_| it.to_owned());
    resolved(&this) == resolved(&kept)
}

/// Outside a pinned workspace (`manifest` its root manifest, when there is one): runs the
/// default `blaze update` installed ([`update::runs_unpinned`]) with `words` when that is not
/// this one, fetching it first when it is not there, telling it it is the default
/// ([`update::RUN_AS_DEFAULT`]), which every process under it inherits and reads in place of
/// the file. A build of the tree runs itself. `Err` is why the default can't run, and no other
/// runs in its place: `blaze update` or removing the default says otherwise.
fn run_default(words: &[OsString], manifest: Option<std::path::PathBuf>) -> Result<Ran, String> {
    let runs = update::runs_unpinned().map_err(|why| why.to_string())?;
    if runs == pin::OURS {
        return Ok(Ran::Here { pin: Pinned::None, manifest });
    }
    // Run as the default, and not it: the `blaze` kept for the default is another release.
    // Only then: a blaze found on PATH under the default (its daemon, a build script, a test)
    // inherits `BLAZE_RUN_AS_DEFAULT` too, and runs the default again, which runs itself.
    if std::env::var_os(update::RUN_AS_DEFAULT).is_some_and(|it| it == runs.as_str())
        && is_kept_for(&runs)
    {
        let this = std::env::current_exe().unwrap_or_default();
        return Err(format!(
            "blaze {runs} is the default `blaze update` installed, and {}, which a blaze ran as it (`{}`), is \
             blaze {}: removing {} has it fetched again",
            this.display(),
            update::RUN_AS_DEFAULT,
            pin::OURS,
            this.parent().unwrap_or(Path::new("")).display()
        ));
    }
    let default = versions::fetch_for(&runs, "the default `blaze update` installed").map_err(|why| {
        format!(
            "blaze {runs}, the default `blaze update` installed, could not be fetched ({why:#}); no other blaze \
             runs in its place (`blaze update` installs the newest)"
        )
    })?;
    if matches!(words, [it] if it == "--version" || it == "-V") {
        println!("blaze {} runs blaze {runs}, the default `blaze update` installed:", pin::OURS);
    }
    if says_updates(words) {
        update::say(&runs, update::Pin::None, manifest.as_deref());
    }
    let mut command = std::process::Command::new(&default);
    command.args(words).env(update::RUN_AS_DEFAULT, &runs);
    Ok(Ran::Ended(ended(&mut command, &default)))
}

/// Why the release a channel names can't be told.
#[derive(Debug)]
enum ChannelError {
    /// No home directory, where blaze keeps what a channel named.
    NoHome,
    /// The channel was not read within [`pin::CHANNEL_FRESH`] and can't be read again. `last`
    /// is what it named when last read, which does not run in its place.
    Unread { why: anyhow::Error, last: Option<String> },
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelError::NoHome => f.write_str(
                "blaze keeps what a channel names in .blaze in your home directory, and there is \
                 none (`BLAZE_HOME` names where)",
            ),
            ChannelError::Unread { why, last: None } => {
                write!(f, "what it names could not be read: {why:#}")
            }
            ChannelError::Unread { why, last: Some(last) } => write!(
                f,
                "what it names could not be read again ({why:#}); blaze {last}, which it named \
                 when last read, is not run in its place: `blaze version-resolve` reads it again"
            ),
        }
    }
}

/// The version `channel` names: the one read for it within [`pin::CHANNEL_FRESH`], else read
/// again ([`versions::resolve`], which keeps it). One that can't be read again is an error.
fn channel_version(channel: &str) -> Result<String, ChannelError> {
    let file = pin::channel_file(channel).ok_or(ChannelError::NoHome)?;
    let known = pin::channel_known(&file);
    if let Some((version, true)) = known {
        return Ok(version);
    }
    versions::resolve(channel)
        .map_err(|why| ChannelError::Unread { why, last: known.map(|(version, _)| version) })
}

fn failed(why: &str) -> ExitCode {
    eprintln!("error: {why}");
    ExitCode::FAILURE
}

/// Becomes `command`, where a process can, else runs it and ends as it ended.
fn ended(command: &mut std::process::Command, program: &Path) -> ExitCode {
    #[cfg(unix)]
    let error = std::os::unix::process::CommandExt::exec(command);
    #[cfg(not(unix))]
    let error = match command.status() {
        Ok(status) => return ExitCode::from(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1)),
        Err(error) => error,
    };
    failed(&format!("could not run {}: {error}", program.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pins_own_commands_run_this_blaze() {
        for first in OWN {
            assert!(
                matches!(pinned_elsewhere(&[OsString::from(first)], false), Ok(Ran::Here { .. })),
                "{first}"
            );
        }
    }
}
