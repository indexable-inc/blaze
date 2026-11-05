//! The providers that are programs: a credential process spoken to in cargo's protocol
//! (`src/util/credential/process.rs`), and `cargo:token-from-stdout`'s command, which prints a
//! token (`src/util/credential/adaptor.rs`).

use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
};

use anyhow::Context;

use super::{Action, CacheControl, Failure, Response, protocol};

/// `program`, to run where cargo was started, as cargo runs its providers.
#[expect(clippy::disallowed_methods, reason = "the directory is the run's, set here")]
fn program(program: &str, cwd: &Path) -> Command {
    let mut command = Command::new(program);
    command.current_dir(cwd);
    command
}

/// Asks the credential process `path`, started with `--cargo-plugin`; its standard error is
/// the run's.
pub(super) fn credential_process(
    path: &str,
    registry: &protocol::Registry<'_>,
    action: &Action<'_>,
    args: &[&str],
    cwd: &Path,
) -> Result<Response, Failure> {
    let mut child = program(path, cwd)
        .arg("--cargo-plugin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to spawn credential process")?;
    match ask(path, &mut child, registry, action, args) {
        Ok(answer) => answer,
        Err(e) => {
            _ = child.kill();
            Err(Failure::Fatal(e))
        }
    }
}

fn ask(
    path: &str,
    child: &mut Child,
    registry: &protocol::Registry<'_>,
    action: &Action<'_>,
    args: &[&str],
) -> anyhow::Result<Result<Response, Failure>> {
    let mut from = BufReader::new(child.stdout.take().expect("piped"));
    let mut to = child.stdin.take().expect("piped");
    let mut line = String::new();
    from.read_line(&mut line).context("failed to read hello from credential provider")?;
    protocol::hello(&line)?;
    let request =
        protocol::request(registry, action, args).context("failed to serialize request")?;
    writeln!(to, "{request}").context("failed to write to credential provider")?;
    line.clear();
    from.read_line(&mut line).context("failed to read response from credential provider")?;
    let answer = protocol::answer(&line).context("failed to deserialize response")?;
    drop(to);
    let status = child.wait().context("credential process never started")?;
    if !status.success() {
        anyhow::bail!("credential process `{path}` failed with status {status}`");
    }
    Ok(answer)
}

/// `cargo:token-from-stdout`: `args` a command and its arguments, `{index_url}` in them the
/// registry's, run for a token on the one line it prints; it does nothing else.
pub(super) fn token_from_stdout(
    registry: &protocol::Registry<'_>,
    action: &Action<'_>,
    args: &[&str],
    cwd: &Path,
) -> Result<Response, Failure> {
    if !matches!(action, Action::Get(_)) {
        return Err(Failure::operation_not_supported());
    }
    let (program, args) = args.split_first().ok_or_else(|| {
        anyhow::anyhow!(
            "The first argument to `cargo:token-from-stdout` must be a command that prints a \
             token on stdout"
        )
    })?;
    let mut command = self::program(program, cwd);
    command
        .args(args.iter().map(|it| it.replace("{index_url}", registry.index_url)))
        .env("CARGO_REGISTRY_INDEX_URL", registry.index_url)
        .stdout(Stdio::piped());
    if let Some(name) = registry.name {
        command.env("CARGO_REGISTRY_NAME_OPT", name);
    }
    let mut child = command.spawn().context("failed to spawn credential process")?;
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("piped")
        .read_to_string(&mut out)
        .context("failed to read from credential provider")?;
    if let Some(end) = out.find('\n') {
        if out.len() > end + 1 {
            return Err(Failure::Fatal(anyhow::anyhow!(
                "process `{program}` returned more than one line of output; expected a single token"
            )));
        }
        out.truncate(end);
        if out.ends_with('\r') {
            out.pop();
        }
    }
    let status = child.wait().context("credential process never started")?;
    if !status.success() {
        return Err(Failure::Fatal(anyhow::anyhow!(
            "process `{program}` failed with status `{status}`"
        )));
    }
    Ok(Response::Get { token: out, cache: CacheControl::Session, operation_independent: true })
}
