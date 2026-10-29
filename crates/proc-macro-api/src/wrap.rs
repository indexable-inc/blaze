//! A process-wide hook that wraps the command a proc-macro server is started with, so that a
//! caller can start every server in a sandbox (blaze's `blaze sandbox SPEC -- SERVER`). The
//! server's own `--version` question runs no macro and is not wrapped.

use std::{io, process::Command, sync::OnceLock};

/// What a server is started with when the caller asked for it to run outside a wrapper: the
/// wrapper is given the command with it set, and leaves such a command as it is.
pub const UNSANDBOXED: &str = "BLAZE_PROC_MACROS_UNSANDBOXED";

/// Makes the command a server is started with of the one it would be started with, or says
/// why it can't: the server then does not start (there is no starting it unwrapped instead).
pub type Wrap = fn(Command) -> io::Result<Command>;

static WRAP: OnceLock<Wrap> = OnceLock::new();
static INHERITED: OnceLock<fn(&str) -> bool> = OnceLock::new();

/// Restricts which inherited request variables a sandboxed server receives. Explicit
/// configuration overrides and the crate's expansion variables keep their own scope.
pub fn inherited_env_with(kept: fn(&str) -> bool) {
    _ = INHERITED.set(kept);
}

pub fn inherits(name: &str) -> bool {
    INHERITED.get().is_none_or(|kept| kept(name))
}

/// Has every proc-macro server started from here on be started with what `wrap` makes of its
/// command. The first call is the one kept.
pub fn wrap_servers_with(wrap: Wrap) {
    _ = WRAP.set(wrap);
}

pub(crate) fn wrapped(command: Command) -> io::Result<Command> {
    match WRAP.get() {
        Some(wrap) => wrap(command),
        None => Ok(command),
    }
}
