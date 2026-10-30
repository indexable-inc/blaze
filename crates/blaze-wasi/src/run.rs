//! A build script's module under Wasmtime: WASI preview 1 with the directories and environment
//! it is given and nothing else (no network, no other files), and `blaze.run_tool` answered by
//! the caller.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use wasmtime::{Caller, Engine, Linker, Module, Store};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{FsPerms, I32Exit, WasiCtxBuilder};

use crate::{IMPORT_MODULE, Request, Response};

/// A directory of the host the script sees, at `guest`.
pub struct Preopen {
    pub host: PathBuf,
    pub guest: String,
    pub writable: bool,
}

/// One run of a script's module.
pub struct Guest<'a> {
    pub module: &'a Path,
    /// Its arguments, the first its name.
    pub args: &'a [String],
    pub env: &'a [(String, String)],
    pub preopens: &'a [Preopen],
}

/// How a run ended.
pub struct Exited {
    /// What it passed to `proc_exit`, 0 when `main` returned.
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The trap that ended it, a panic's `unreachable` among them, with Wasmtime's backtrace.
    pub trap: Option<String>,
}

/// The most a script may print to each of stdout and stderr.
const MOST_PRINTED: usize = 256 << 20;

struct State {
    wasi: WasiP1Ctx,
    tool: Box<dyn FnMut(Request) -> Response + Send>,
    response: Vec<u8>,
}

/// The engine every run shares: compiling a module is most of a short run's time.
fn engine() -> &'static Engine {
    static ENGINE: std::sync::OnceLock<Engine> = std::sync::OnceLock::new();
    ENGINE.get_or_init(Engine::default)
}

/// Compiles the module at `path` for this host.
pub fn compile(path: &Path) -> anyhow::Result<Module> {
    Module::from_file(engine(), path).map_err(anyhow::Error::from).with_context(|| format!("compiling {}", path.display()))
}

/// Runs `guest`, `module` being its module compiled ([`compile`]), with each `blaze.run_tool`
/// answered by `tool`.
pub fn run(
    guest: &Guest<'_>,
    module: &Module,
    tool: impl FnMut(Request) -> Response + Send + 'static,
) -> anyhow::Result<Exited> {
    let stdout = MemoryOutputPipe::new(MOST_PRINTED);
    let stderr = MemoryOutputPipe::new(MOST_PRINTED);
    let mut wasi = WasiCtxBuilder::new();
    wasi.args(guest.args).envs(guest.env).stdout(stdout.clone()).stderr(stderr.clone());
    wasi.allow_blocking_current_thread(true);
    wasi.allow_tcp(false).allow_udp(false).allow_ip_name_lookup(false);
    for preopen in guest.preopens {
        let perms = if preopen.writable { FsPerms::ReadWrite } else { FsPerms::ReadOnly };
        wasi.preopened_dir(&preopen.host, &preopen.guest, perms)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("opening {} for the script", preopen.host.display()))?;
    }
    let state = State { wasi: wasi.build_p1(), tool: Box::new(tool), response: Vec::new() };
    let mut store = Store::new(engine(), state);
    let mut linker: Linker<State> = Linker::new(engine());
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |it: &mut State| &mut it.wasi)?;
    linker.func_wrap(
        IMPORT_MODULE,
        "run_tool",
        |mut caller: Caller<'_, State>, at: u32, len: u32| -> wasmtime::Result<i64> {
            let memory = memory(&mut caller)?;
            let mut bytes = vec![0; len as usize];
            if memory.read(&caller, at as usize, &mut bytes).is_err() {
                return Ok(-1);
            }
            let Some(request) = Request::read(&bytes) else { return Ok(-1) };
            let response = (caller.data_mut().tool)(request).bytes();
            let len = response.len() as i64;
            caller.data_mut().response = response;
            Ok(len)
        },
    )?;
    linker.func_wrap(
        IMPORT_MODULE,
        "run_tool_response",
        |mut caller: Caller<'_, State>, at: u32, len: u32| -> wasmtime::Result<()> {
            let memory = memory(&mut caller)?;
            let response = std::mem::take(&mut caller.data_mut().response);
            if response.len() != len as usize {
                return Err(wasmtime::Error::msg("run_tool_response: a wrong length"));
            }
            memory.write(&mut caller, at as usize, &response)?;
            Ok(())
        },
    )?;
    let instance = linker.instantiate(&mut store, module)?;
    let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
    let (status, trap) = match start.call(&mut store, ()) {
        Ok(()) => (0, None),
        Err(error) => match error.downcast_ref::<I32Exit>() {
            Some(exit) => (exit.0, None),
            None => (1, Some(format!("{error:?}"))),
        },
    };
    drop(store);
    Ok(Exited {
        status,
        stdout: stdout.contents().to_vec(),
        stderr: stderr.contents().to_vec(),
        trap,
    })
}

fn memory(caller: &mut Caller<'_, State>) -> wasmtime::Result<wasmtime::Memory> {
    caller
        .get_export("memory")
        .and_then(|it| it.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("the script exports no memory"))
}
