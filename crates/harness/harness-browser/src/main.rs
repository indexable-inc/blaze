//! `harness-browser`: the harness's embedded Chromium (README.md). One binary is both CEF's browser process (`Harness
//! Browser.app/Contents/MacOS/Harness Browser`, started by the daemon) and every helper (`... Helper (Renderer).app` and
//! the rest, started by Chromium with `--type=`).
//!
//! Modules:
//! - host: the UI thread's state (views, connections, the app's port), the command queue, every step.
//! - handlers: CEF's interfaces (app, browser process, per-view client and handlers, DevTools observer, tasks,
//!   the manage window's delegates).
//! - kernel: the Unix socket (accept, per-connection reader and writer threads).
//! - app_link: the app's Mach messages.
//! - ring: Metal blits into each watched view's ring of IOSurfaces.
//! - cdp_ids, keys: sans-IO cores (DevTools id renumbering; Mac key codes to CEF key events).
//! - extensions: the profile's installed extensions.
//! - manage: the Chrome-style manage window.
//! - app_class: the NSApplication subclass CEF requires on macOS.
//! - parent: the kqueue watch on the daemon's pid.
mod app_class;
mod app_link;
mod cdp_ids;
mod extensions;
mod handlers;
mod host;
mod kernel;
mod keys;
mod location;
mod manage;
mod parent;
mod places;
mod ring;
mod slots;

use cef::*;
use std::ffi::{CString, c_char, c_int, c_void};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn main() {
    let args = args::Args::new();
    let helper = std::env::args().skip(1).any(|a| a.starts_with("--type="));
    let r = if helper { helper_main(&args) } else { browser_main(&args) };
    match r {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            if helper {
                eprintln!("harness-browser helper failed: {e}");
            } else {
                // The daemon reads this line on stdout (crates/harness/harness/src/browser.rs).
                println!("harness-browser failed: {e}");
                eprintln!("harness-browser failed: {e}");
            }
            std::process::exit(1);
        }
    }
}

fn exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("finding this executable's path: {e}"))
}

/// Load the framework at `rel` from the executable's directory, checking it is there first (cef-rs's loader panics on
/// a path that does not resolve).
fn load_framework(exe: &Path, helper: bool) -> Result<library_loader::LibraryLoader, String> {
    let rel = if helper { "../../.." } else { "../Frameworks" };
    let dir = exe.parent().ok_or_else(|| format!("{} has no parent directory", exe.display()))?;
    let lib = dir.join(rel).join("Chromium Embedded Framework.framework/Chromium Embedded Framework");
    if !lib.exists() {
        return Err(format!(
            "the Chromium Embedded Framework is not at {} (this binary must run from inside Harness Browser.app: \
             uv run --frozen crates/harness/scripts/cef.py '{{\"op\": \"bundle\", \"profile\": \"release\"}}')",
            lib.display()
        ));
    }
    let loader = library_loader::LibraryLoader::new(exe, helper);
    if !loader.load() {
        return Err(format!("cef_load_library refused {}", lib.display()));
    }
    // Pins the C API version the bindings were generated for (CEF checks it against the framework).
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    Ok(loader)
}

// ---------------------------------------------------------------- helpers

/// The helper's sandbox context (`cef_sandbox_initialize`'s), kept for the process's life.
struct Sandbox {
    _ctx: *mut c_void,
}

/// Initialise the macOS sandbox before the framework loads, with `libcef_sandbox.dylib` beside the framework.
fn sandbox(exe: &Path, args: &MainArgs) -> Result<Sandbox, String> {
    let dir = exe.parent().ok_or_else(|| format!("{} has no parent directory", exe.display()))?;
    let lib = dir.join("../../../Chromium Embedded Framework.framework/Libraries/libcef_sandbox.dylib");
    let c = CString::new(lib.as_os_str().as_encoded_bytes()).map_err(|_| format!("{} has a NUL byte", lib.display()))?;
    // SAFETY: dlopen of a path we built; the handle is kept for the process's life (never dlclosed).
    let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if h.is_null() {
        // SAFETY: dlerror's thread-local message, read at once.
        let why = unsafe { std::ffi::CStr::from_ptr(libc::dlerror()) }.to_string_lossy().into_owned();
        return Err(format!("loading the sandbox library {}: {why}", lib.display()));
    }
    // SAFETY: a symbol of the handle just opened.
    let sym = unsafe { libc::dlsym(h, c"cef_sandbox_initialize".as_ptr()) };
    if sym.is_null() {
        return Err(format!("{} has no cef_sandbox_initialize", lib.display()));
    }
    // SAFETY: cef_sandbox.h's `void* cef_sandbox_initialize(int argc, char** argv)`.
    let init: extern "C" fn(c_int, *mut *mut c_char) -> *mut c_void = unsafe { std::mem::transmute(sym) };
    let ctx = init(args.argc, args.argv);
    if ctx.is_null() {
        return Err(format!("cef_sandbox_initialize ({}) refused to sandbox this helper", lib.display()));
    }
    Ok(Sandbox { _ctx: ctx })
}

fn helper_main(args: &args::Args) -> Result<i32, String> {
    let exe = exe()?;
    let _sandbox = sandbox(&exe, args.as_main_args())?;
    let _loader = load_framework(&exe, true)?;
    let code = execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());
    if code < 0 {
        return Err(format!("started with --type= but cef_execute_process returned {code} (not a CEF sub-process)"));
    }
    Ok(code)
}

// ---------------------------------------------------------------- the browser process

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|e| format!("{name} is not set ({e}); the harness daemon starts this host with it"))
}

fn cef_path(p: &Path) -> Result<CefString, String> {
    p.to_str().map(CefString::from).ok_or_else(|| format!("{} is not UTF-8, which CEF's settings cannot carry", p.display()))
}

fn browser_main(args: &args::Args) -> Result<i32, String> {
    let home = PathBuf::from(env("HARNESS_HOME")?);
    let pid_text = env("HARNESS_DAEMON_PID")?;
    let daemon_pid: i32 = pid_text.parse().map_err(|e| format!("HARNESS_DAEMON_PID={pid_text:?} is not a pid: {e}"))?;
    let dir = harness_proto::browser::dir(&home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| format!("chmod 0700 {}: {e}", dir.display()))?;
    let profile = dir.join("profile");
    std::fs::create_dir_all(&profile).map_err(|e| format!("creating {}: {e}", profile.display()))?;

    let exe = exe()?;
    let _loader = load_framework(&exe, false)?;
    app_class::install()?;
    host::configure(host::Config { home, dir: dir.clone(), daemon_pid })?;

    let settings = Settings {
        no_sandbox: 0,
        windowless_rendering_enabled: 1,
        root_cache_path: cef_path(&dir)?,
        cache_path: cef_path(&profile)?,
        persist_session_cookies: 1,
        log_file: cef_path(&dir.join("cef.log"))?,
        remote_debugging_port: 0,
        keychain_service_name: CefString::from("Harness Browser Safe Storage"),
        keychain_account_name: CefString::from("Harness Browser"),
        ..Default::default()
    };
    let mut app = handlers::app();
    if initialize(Some(args.as_main_args()), Some(&settings), Some(&mut app), std::ptr::null_mut()) != 1 {
        return Err(format!("cef_initialize failed (its log: {})", dir.join("cef.log").display()));
    }
    // Waits for: quit_message_loop (host::stop_loop: the daemon exited, a Quit, or every view closed after one).
    run_message_loop();
    host::teardown();
    shutdown();
    Ok(0)
}
