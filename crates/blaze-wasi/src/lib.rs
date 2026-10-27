//! Build scripts compiled for wasm32-wasip1 and run under Wasmtime, an experiment beside the
//! native sandbox (docs/cargo.md, "Build scripts under WASI").
//!
//! - [`STD_PROCESS`] is blaze's `std::process` for such a script: the standard library ra-jit
//!   compiles for it (`ra-jit --run-tool`) has it in place of
//!   `library/std/src/sys/process/unsupported.rs`, and its commands go to the host through the
//!   import `blaze.run_tool`.
//! - [`Request`] and [`Response`] are what that import is given and answers, as
//!   `std/process.rs` writes and reads them.
//! - With the feature `run`, [`run::run`] runs a script's module under Wasmtime with WASI
//!   preview 1, its preopens and environment, and answers `blaze.run_tool` with what the
//!   caller says.

#[cfg(feature = "run")]
pub mod run;

/// blaze's `sys/process` module for wasm32-wasip1 build scripts.
pub const STD_PROCESS: &str = include_str!("../std/process.rs");

/// Where in `library/std/src` [`STD_PROCESS`] stands.
pub const STD_PROCESS_AT: &str = "sys/process/unsupported.rs";

/// The SHA-256 of the toolchain's file at [`STD_PROCESS_AT`] that [`STD_PROCESS`] was written
/// against (nightly-2026-10-25): it uses the module's private interface to `std::process`, so
/// a toolchain whose file differs needs it written again, and a build with it is refused.
pub const STD_PROCESS_FOR: &str =
    "26313906a12d1cebdd61925039ba7e81e2d4e261ab572cee0ef682c3e000a184";

/// The wasm import module of the host's functions.
pub const IMPORT_MODULE: &str = "blaze";

/// One command a script asked the host to run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub program: Vec<u8>,
    pub cwd: Option<Vec<u8>>,
    pub args: Vec<Vec<u8>>,
    /// The whole environment the command runs with, as std worked it out.
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    pub stdin: Vec<u8>,
}

/// What the host answers a [`Request`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Ran { status: i32, stdout: Vec<u8>, stderr: Vec<u8> },
    /// The program is not one a script may run; the message names it.
    Refused(String),
    /// It could not be started.
    NotStarted(String),
}

impl Request {
    /// The request `std/process.rs` wrote, `None` when it is not one.
    pub fn read(bytes: &[u8]) -> Option<Request> {
        let rest = &mut &*bytes;
        let program = field(rest)?.to_vec();
        let cwd = field(rest)?;
        let cwd = (!cwd.is_empty()).then(|| cwd.to_vec());
        let mut args = Vec::new();
        for _ in 0..count(rest)? {
            args.push(field(rest)?.to_vec());
        }
        let mut env = Vec::new();
        for _ in 0..count(rest)? {
            env.push((field(rest)?.to_vec(), field(rest)?.to_vec()));
        }
        let stdin = field(rest)?.to_vec();
        rest.is_empty().then_some(Request { program, cwd, args, env, stdin })
    }

    /// The bytes `std/process.rs` writes of it.
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put(&mut out, &self.program);
        put(&mut out, self.cwd.as_deref().unwrap_or_default());
        out.extend((self.args.len() as u32).to_le_bytes());
        for arg in &self.args {
            put(&mut out, arg);
        }
        out.extend((self.env.len() as u32).to_le_bytes());
        for (name, value) in &self.env {
            put(&mut out, name);
            put(&mut out, value);
        }
        put(&mut out, &self.stdin);
        out
    }
}

impl Response {
    /// The bytes `std/process.rs` reads.
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Response::Ran { status, stdout, stderr } => {
                out.push(0);
                out.extend(status.to_le_bytes());
                put(&mut out, stdout);
                put(&mut out, stderr);
            }
            Response::Refused(message) => {
                out.push(1);
                put(&mut out, message.as_bytes());
            }
            Response::NotStarted(message) => {
                out.push(2);
                put(&mut out, message.as_bytes());
            }
        }
        out
    }
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend((bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn count(rest: &mut &[u8]) -> Option<usize> {
    let (head, tail) = rest.split_at_checked(4)?;
    *rest = tail;
    Some(u32::from_le_bytes(head.try_into().ok()?) as usize)
}

fn field<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = count(rest)?;
    let (head, tail) = rest.split_at_checked(len)?;
    *rest = tail;
    Some(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_reads_back_as_it_was_written() {
        let request = Request {
            program: b"cc".to_vec(),
            cwd: Some(b"/pkg".to_vec()),
            args: vec![b"-c".to_vec(), b"a.c".to_vec()],
            env: vec![(b"OUT_DIR".to_vec(), b"/out".to_vec())],
            stdin: b"int a;".to_vec(),
        };
        assert_eq!(Request::read(&request.bytes()), Some(request.clone()));
        let no_cwd = Request { cwd: None, ..request };
        assert_eq!(Request::read(&no_cwd.bytes()), Some(no_cwd.clone()));
        assert_eq!(Request::read(&no_cwd.bytes()[1..]), None);
    }

    #[test]
    fn the_std_module_names_the_import_module() {
        assert!(STD_PROCESS.contains(&format!("wasm_import_module = \"{IMPORT_MODULE}\"")));
        assert!(STD_PROCESS.contains("link_name = \"run_tool\""));
        assert!(STD_PROCESS.contains("link_name = \"run_tool_response\""));
    }
}
