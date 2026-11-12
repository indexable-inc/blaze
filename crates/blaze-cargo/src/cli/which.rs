//! Which blaze answered: what `blaze --version` says of this binary beyond its release line,
//! and the one line a command says when the daemon it is about to ask is another build of blaze
//! than itself.
//!
//! A command that may start a daemon ([`super::daemon::Start::IfMissing`]: a build, a check, a
//! test) already stops a daemon of another build and starts its own, saying so
//! (`daemon::connect`). A command that only asks ([`super::daemon::Start::Never`]: `blaze slow`,
//! `why`, `explain`, `debug`, `status`) never did: it asked whatever daemon listened, and on
//! 2026-11-10 that was twice the daemon of an older binary, whose numbers were then reported as
//! the new binary's. Such a command still asks it (its history is the one there is, and stopping
//! a daemon is not a reader's to do), and [`warn_if_other`] says first whose answer follows.

use std::path::Path;

/// `blaze --version`'s text: the release line as it always was (tools read `blaze <version>`
/// from the first line), then this binary's identity: the BLAKE3 of its bytes, which every
/// build it answers records as `builds.blaze`, and its file. A binary that can't be read says
/// so on that line; the release line stands either way.
pub fn version_text(release: &str, binary: Result<(&str, &Path), &str>) -> String {
    match binary {
        // alloc: the text, once
        Ok((blake3, exe)) => format!("{release}\nbinary: {blake3} {}\n", exe.display()),
        // alloc: the text, once
        Err(why) => format!("{release}\nbinary: not read ({why})\n"),
    }
}

/// [`version_text`] of this process.
pub fn version() -> String {
    let release = crate::version();
    let exe = std::env::current_exe().map_err(|error| format!("finding blaze's own executable: {error}"));
    let hash = blaze_profile::identity::binary().map_err(|error| format!("hashing blaze itself: {error}"));
    match (&exe, &hash) {
        (Ok(exe), Ok(hash)) => version_text(&release, Ok((hash.as_str(), exe.as_path()))),
        (Err(why), _) | (_, Err(why)) => version_text(&release, Err(why.as_str())),
    }
}

/// A build as a line names it: its file and the first 12 digits of its build id.
fn named(build: &blaze_api::Build) -> String {
    let id = format!("{:x}", build.id); // alloc: a line, once a command
    format!("{} {}", build.exe.path, &id[..id.len().min(12)])
}

/// The line a command that only asks says when the daemon answering is another build of blaze.
pub fn other_said(pid: u32, theirs: &blaze_api::Build, ours: &blaze_api::Build) -> String {
    format!(
        "the daemon (pid {pid}) is blaze {}, this client is {}: what follows is that daemon's answer, of the builds it \
         ran, not this binary's; a build with this client (`blaze build`) stops it and starts this one",
        named(theirs),
        named(ours)
    )
}

/// What [`warn_if_other`] found, for its test: the line, when the daemon on `api` is another
/// build than `ours` ([`super::daemon::builds`]: nothing is hashed for a daemon of this very
/// file). A daemon of this build keeping other settings is this build: nothing is said.
pub fn other<A: super::daemon::Api>(
    api: &mut A,
    socket: &Path,
    ours: &blaze_api::Executable,
    hash: impl FnOnce() -> anyhow::Result<blaze_api::Build>,
) -> anyhow::Result<Option<String>> {
    Ok(match super::daemon::builds(api, socket, ours, &[], hash)? {
        super::daemon::Builds::Other { pid, theirs, ours } => Some(other_said(pid, &theirs, &ours)),
        _ => None,
    })
}

/// Says on stderr, once, when a daemon is running and is another build of blaze than this
/// process ([`other_said`]); nothing when none runs or it is this build. One Status round trip
/// on a connection of its own, before the command's request. A daemon that can't be asked which
/// blaze it is (another API schema) is said too, as a warning: the command's own request then
/// fails or not on its own.
#[cfg(all(any(unix, windows), feature = "native"))]
pub fn warn_if_other(color: bool) {
    use anyhow::Context as _;
    let asked = (|| -> anyhow::Result<Option<String>> {
        let Some(mut client) = super::daemon::connect(super::daemon::Start::Never)? else {
            return Ok(None);
        };
        let _span = tracing::info_span!(
            target: "blaze",
            "client: ask the daemon which blaze it is",
        )
        .entered();
        let socket = crate::serve::socket()?;
        let exe = std::env::current_exe().context("finding blaze's own executable")?;
        let file = std::fs::File::open(&exe)
            .with_context(|| format!("reading blaze itself, {}", exe.display()))?;
        let ours = crate::serve::executable(&exe, &file)?;
        other(&mut client, &socket, &ours, || crate::serve::build_of(&exe))
    })();
    match asked {
        Ok(None) => {}
        Ok(Some(line)) => eprint!("{}", super::say::warning(&line, color)),
        Err(error) => eprint!(
            "{}",
            super::say::warning(&format!("which blaze the daemon is was not learned: {error:#}"), color)
        ),
    }
}

/// Where there is no daemon to ask (the seed, a platform with no socket), nothing is said.
#[cfg(not(all(any(unix, windows), feature = "native")))]
pub fn warn_if_other(_: bool) {}

#[cfg(all(test, feature = "native"))]
mod tests {
    use blaze_api::{Request, Response, ResponseRef, Status};

    use super::*;
    use crate::cli::daemon::Api;
    use blaze_api::client::ClientError;

    fn build(id: u8, path: &str, inode: u64) -> blaze_api::Build {
        blaze_api::Build {
            id: blaze_ids::ContentId([id; 32]),
            exe: blaze_api::Executable { path: path.to_owned(), len: 1 << 20, modified_ns: 5, inode },
        }
    }

    /// A daemon whose Status says `build`; every request it is sent is kept.
    struct OfBuild {
        build: blaze_api::Build,
        asked: Vec<Request>,
    }

    impl Api for OfBuild {
        fn call<F: FnMut(ResponseRef<'_>, bool)>(
            &mut self,
            request: Request,
            on: F,
        ) -> Result<bool, ClientError> {
            let answer = match request {
                Request::Status => vec![
                    Response::Status(Status {
                        pid: 77,
                        requests: Vec::new(),
                        settings: Vec::new(),
                        views: 0,
                        history: None,
                        memory_bytes: 0,
                        budget_bytes: 0,
                        uptime_ms: 40,
                        placed_by: None,
                        build: self.build.clone(),
                        bugs: Vec::new(),
                        builds: Vec::new(),
                        downloads: Vec::new(),
                    }),
                    Response::Done { ok: true },
                ],
                _ => vec![Response::Done { ok: true }],
            };
            self.asked.push(request);
            Ok(crate::cli::daemon::tests::hand(&answer, on))
        }
    }

    #[test]
    fn the_version_names_the_binary_after_the_release_line() {
        let hash = "ab".repeat(32);
        let said = version_text("blaze 0.0.0-dev (fb82b09)", Ok((&hash, Path::new("/opt/blaze"))));
        assert_eq!(said, format!("blaze 0.0.0-dev (fb82b09)\nbinary: {hash} /opt/blaze\n"));
        assert_eq!(said.lines().next(), Some("blaze 0.0.0-dev (fb82b09)"), "what tools read is unchanged");
        assert_eq!(
            version_text("blaze 1 (a)", Err("hashing blaze itself: gone")),
            "blaze 1 (a)\nbinary: not read (hashing blaze itself: gone)\n"
        );
    }

    /// A reader asking the daemon of an older binary (2026-11-10: a stage-1 build had failed
    /// and the old daemon went on answering) says whose answer follows, and asks that daemon
    /// nothing but its Status: it is not stopped by a command that only reads.
    #[test]
    fn a_daemon_of_another_build_is_named_before_its_answer() {
        let (a, b) = (build(1, "/old/blaze", 10), build(2, "/new/blaze", 11));
        let mut daemon = OfBuild { build: a, asked: Vec::new() };
        let said = other(&mut daemon, Path::new("/run/blaze/v2.sock"), &b.exe, || Ok(b.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(
            said,
            "the daemon (pid 77) is blaze /old/blaze 010101010101, this client is /new/blaze 020202020202: what \
             follows is that daemon's answer, of the builds it ran, not this binary's; a build with this client \
             (`blaze build`) stops it and starts this one"
        );
        assert_eq!(daemon.asked, [Request::Status]);
    }

    #[test]
    fn a_daemon_of_this_build_says_nothing() {
        let a = build(1, "/opt/blaze", 10);
        let mut daemon = OfBuild { build: a.clone(), asked: Vec::new() };
        let said = other(&mut daemon, Path::new("/run/blaze/v2.sock"), &a.exe, || {
            panic!("hashed this blaze for an unchanged file")
        });
        assert_eq!(said.unwrap(), None);
    }
}
