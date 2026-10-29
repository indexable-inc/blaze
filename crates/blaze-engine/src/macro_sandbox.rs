//! Every proc-macro server a load starts, the toolchain's and blaze's own, runs in the
//! sandbox tests and build scripts run in ([`crate::sandbox`]): the machine read-only, empty
//! temporary directories, no network but its own loopback, nothing written. A macro reads
//! what it reads (the workspace, the machine) and writes nothing: what it makes is the tokens
//! it hands back. The server is started once, when a load first needs it, and serves every
//! expansion after, so the sandbox costs its start and nothing per expansion.
//!
//! A package named in `unsandboxed-proc-macros` of the workspace's root manifest
//! ([`sandboxed::WORKSPACE_TABLE`]) has its macros expanded by servers of their own, started
//! outside; `BLAZE_SANDBOX=off` starts every server outside.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
};

use sha2::{Digest as _, Sha256};

use crate::{
    cargo::sandboxed,
    sandbox::{self, Spec},
};

/// The directories the libraries of the loads' proc macros are in, and the loads'
/// workspaces: a server sees them though they are below `/tmp`, which a sandbox has empty.
static SEEN: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// Has every proc-macro server be started in the sandbox, but those of the macros the root
/// manifest of their workspace takes out.
pub(crate) fn install() {
    proc_macro_api::wrap::inherited_env_with(|name| !sandbox::on() || given_to_server(name));
    proc_macro_api::wrap::wrap_servers_with(wrapped);
    load_cargo::unsandboxed_macros_with(Box::new(|workspace, crate_name, library| {
        let (workspace, library): (&Path, &Path) = (workspace.as_ref(), library.as_ref());
        if let Ok(mut seen) = SEEN.lock() {
            seen.insert(workspace.to_owned());
            seen.extend(library.parent().map(Path::to_owned));
        }
        let out = sandboxed::workspace_root(workspace)
            .and_then(|root| root.map(|root| sandboxed::workspace_opt_out(&root)).transpose());
        match out {
            Ok(out) => out.is_some_and(|it| it.macro_outside(crate_name)),
            // A root manifest that says it wrongly takes nothing out; the build reads it too,
            // and says so.
            Err(_) => false,
        }
    }));
}

/// `server`, the command a proc-macro server is started with, as `blaze sandbox SPEC --
/// SERVER`. When the sandbox can't be made the server does not start, and the load says why
/// (`blaze sandbox` prints it); when the command can't be made either, this says why. A server
/// never starts outside instead: only a package taken out, or `BLAZE_SANDBOX=off`, runs one
/// outside.
fn wrapped(server: Command) -> std::io::Result<Command> {
    let outside = server
        .get_envs()
        .any(|(name, value)| name == proc_macro_api::wrap::UNSANDBOXED && value.is_some());
    if outside || !sandbox::on() {
        return Ok(server);
    }
    let refused = |why: String| {
        std::io::Error::other(format!(
            "a proc-macro server runs in a sandbox, which can't be made for it: {why}; `{}=off` \
             runs proc-macro servers, build scripts and tests outside a sandbox",
            crate::sandbox::VAR
        ))
    };
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return Err(refused(format!("there is no sandbox on {} yet (#257)", std::env::consts::OS)));
    }
    match spec_of(&server) {
        Ok(spec) => {
            let exe = std::env::current_exe()
                .map_err(|why| refused(format!("finding blaze itself: {why}")))?;
            let mut command = Command::new(exe);
            // The server by where it is: the path it was named by may go through a link in a
            // directory the sandbox has empty (a toolchain linked into `/tmp`), and the
            // directory it is in is the one the spec lets it read.
            let program = std::fs::canonicalize(server.get_program())
                .map_or_else(|_| server.get_program().to_owned(), PathBuf::into_os_string);
            command.arg(sandbox::COMMAND).arg(spec).arg("--").arg(program);
            command.args(server.get_args());
            if let Some(cwd) = server.get_current_dir() {
                command.current_dir(cwd);
            }
            Ok(command)
        }
        Err(why) => Err(refused(format!("{why:#}"))),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_server_is_given_the_toolchain_s_variables_and_no_secret() {
        for name in [
            "PATH",
            "HOME",
            "RUSTUP_TOOLCHAIN",
            "BLAZE_CACHE",
            "RUST_ANALYZER_INTERNALS_DO_NOT_USE",
        ] {
            assert!(super::given_to_server(name), "{name}");
        }
        for name in [
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "SSH_AUTH_SOCK",
            "OPENSSL_DIR",
            "BLAZE_PROC_MACRO_CACHE",
        ] {
            assert!(!super::given_to_server(name), "{name}");
        }
    }
}

/// Whether the variable `name` of this process's environment is given to a proc-macro server:
/// what finds the toolchain and how a failure is shown ([`sandboxed::KEPT`]) and blaze's and
/// rust-analyzer's own, apart from the compiler's expansion-cache mode. Nothing else (a
/// CI's secrets): what a macro reads with `env!` or
/// `std::env::var` is its crate's environment (`CARGO_*`, `OUT_DIR`, a build script's
/// `rustc-env`), which each expansion is sent with.
fn given_to_server(name: &str) -> bool {
    name != "BLAZE_PROC_MACRO_CACHE"
        && (sandboxed::KEPT.contains(&name)
            || name.starts_with("BLAZE_")
            || name.starts_with("RUST_ANALYZER_"))
}

/// The file of the spec `server` runs in, written where only this user reads it: the
/// environment is what [`given_to_server`] keeps of this process's, with what the command sets.
fn spec_of(server: &Command) -> anyhow::Result<PathBuf> {
    let mut env: std::collections::BTreeMap<String, String> =
        std::env::vars().filter(|(name, _)| given_to_server(name)).collect();
    for (name, value) in server.get_envs() {
        let Some(name) = name.to_str() else { continue };
        match value.and_then(|it| it.to_str()) {
            Some(value) => env.insert(name.to_owned(), value.to_owned()),
            None => env.remove(name),
        };
    }
    let cwd = match server.get_current_dir() {
        Some(it) => it.to_owned(),
        None => std::env::current_dir()?,
    };
    let mut readable: BTreeSet<PathBuf> = SEEN.lock().map(|it| it.clone()).unwrap_or_default();
    readable.insert(cwd.clone());
    for name in ["CARGO_TARGET_DIR", "CARGO_HOME"] {
        readable.extend(env.get(name).map(PathBuf::from));
    }
    readable.extend(
        std::fs::canonicalize(server.get_program())
            .ok()
            .and_then(|it| it.parent().map(Path::to_owned)),
    );
    // The innermost is enough: a directory is seen with what is below it.
    let all: Vec<PathBuf> = readable.iter().cloned().collect();
    readable.retain(|it| !all.iter().any(|other| other != it && it.starts_with(other)));
    let spec = Spec {
        cwd,
        env: env.into_iter().collect(),
        overlays: Vec::new(),
        writable: Vec::new(),
        readable: readable.into_iter().filter(|it| it.is_dir()).collect(),
        record: None,
        record_reads: false,
        not_inputs: Vec::new(),
        view: None,
    };
    let bytes = serde_json::to_vec(&spec)?;
    let dir = crate::cargo::daemon::home()?.join("sandbox");
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    let file = dir.join(format!("proc-macro-{:x}.json", Sha256::digest(&bytes)));
    if !file.exists() {
        let writing = file.with_extension(format!("{}", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        std::io::Write::write_all(&mut options.open(&writing)?, &bytes)?;
        std::fs::rename(&writing, &file)?;
    }
    Ok(file)
}
