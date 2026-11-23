//! The run test of `blaze export` on the host: the tiny program (`main` calls `second`, which
//! returns 42, then calls the C library's `exit` with it) exported for this machine, written to a
//! fresh temporary folder, marked executable, and started as a child with a 10 s deadline. Run:
//!
//! ```text
//! cargo run -p blaze-export --example export_smoke
//! ```
//!
//! Expected line: `exported file exited with status 42`. On macOS this is the proof that the
//! load commands, the chained fixups and the ad-hoc signature are what the kernel and dyld
//! accept (the facts `macho.rs` marks "to verify"); on Linux arm64, that `ld.so` takes the ELF.
//!
//! An example is the edge, a driver: it names the process clock, the file system and the
//! process launcher through `std`, because bstd has no public call yet that starts a child and
//! waits for it with a deadline (blaze-core's `Effect::Spawn` needs a driver loop). The library
//! it drives is sans-IO.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use blaze_code::record::{CodeRecord, CodeView, Site, SiteKind, Space, Symbol, Target};
use blaze_export::{Entry, Function, Panic, Platform, ProgramIn, Tier, export};
use blaze_own::{AVec, Alloc, Vec};

const DEADLINE: Duration = Duration::from_secs(10);

fn rec<'a>(code: &[u32], sites: &[(u32, SiteKind, Target)], outside: &[(u32, &str)], alloc: Alloc<'a>) -> AVec<'a, u8> {
    let mut r = CodeRecord::new_in(alloc);
    for &inst in code {
        r.code.extend_from_slice(&inst.to_le_bytes());
    }
    for &(at, kind, target) in sites {
        let index = r.target(target);
        r.sites.push(Site { at, kind, target: index, addend: 0 });
    }
    for &(at, name) in outside {
        let index = r.outside(name);
        r.sites.push(Site { at, kind: SiteKind::Call, target: index, addend: 0 });
    }
    let mut out = Vec::new_in(alloc);
    match r.seal() {
        Ok(sealed) => sealed.encode(&mut out),
        Err(error) => panic!("the smoke program's record is not a record: {error}"),
    }
    out
}

fn host() -> Option<Platform> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some(Platform::MacosArm64),
        ("linux", "aarch64") => Some(Platform::LinuxArm64),
        _ => None,
    }
}

fn main() -> ExitCode {
    let Some(platform) = host() else {
        eprintln!("export_smoke: this host is not arm64 macOS or arm64 Linux; nothing to run");
        return ExitCode::FAILURE;
    };
    // An entry that takes no grant yet (a plain `fn main`): the run's memory is one frame
    // taken from the OS here and handed down.
    let frame = blaze_mem::Frame::with_reserve(1 << 30, blaze_mem::Tag::Other).expect("the entry's frame");
    let erased = blaze_own::Erased::new(&frame);
    let alloc = erased.handle();
    let func =|name: &str| Target { space: Space::Func, symbol: Symbol::of_name(name) };
    // stp x29, x30, [sp, #-16]!; mov x29, sp; call second; call exit; ldp; ret.
    let main = rec(
        &[
            0xA9BF_7BFD, 0x9100_03FD, 0x9000_0010, 0xF940_0210, 0xD63F_0200, 0x9000_0010, 0xF940_0210, 0xD63F_0200, 0xA8C1_7BFD,
            0xD65F_03C0,
        ],
        &[(8, SiteKind::Call, func("second"))],
        &[(20, "exit")],
        alloc,
    );
    // mov w0, #42; ret.
    let second = rec(&[0x5280_0540, 0xD65F_03C0], &[], &[], alloc);
    fn read(bytes: &[u8]) -> CodeView<'_> {
        match CodeView::read(bytes) {
            Ok(view) => view,
            Err(error) => panic!("the smoke program's record does not read: {error}"),
        }
    }
    let functions = [
        Function { symbol: Symbol::of_name("main"), record: read(&main), tier: Tier::Fast },
        Function { symbol: Symbol::of_name("second"), record: read(&second), tier: Tier::Fast },
    ];
    let program = ProgramIn {
        name: "export-smoke",
        functions: &functions,
        data: &[],
        entry: Entry::CMain { main: Symbol::of_name("main") },
        panic: Panic::Unwind,
        personality: None,
        tls: None,
        profile: None,
        natives: None,
        exports: &[],
    };
    let mut file = Vec::new_in(alloc);
    let exported = match export(&program, platform, &mut file, alloc) {
        Ok(exported) => exported,
        Err(error) => {
            eprintln!("export_smoke: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut report = blaze_own::String::new_in(alloc);
    if exported.report(&mut report).is_ok() {
        print!("{}", &*report);
    }

    let dir = std::env::temp_dir().join(format!("blaze-export-smoke-{}", std::process::id()));
    let path = dir.join("export-smoke");
    let written = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::File::create(&path))
        .and_then(|mut f| f.write_all(&file).map(|()| f))
        .and_then(|f| f.set_permissions(std::fs::Permissions::from_mode(0o755)));
    if let Err(error) = written {
        eprintln!("export_smoke: writing {}: {error}", path.display());
        return ExitCode::FAILURE;
    }
    let mut child = match Command::new(&path).spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("export_smoke: starting {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > DEADLINE => {
                let _ = child.kill();
                eprintln!("export_smoke: {} ran past its {DEADLINE:?} deadline and was killed", path.display());
                return ExitCode::FAILURE;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                eprintln!("export_smoke: waiting for {}: {error}", path.display());
                return ExitCode::FAILURE;
            }
        }
    };
    let _ = std::fs::remove_dir_all(&dir);
    match status.code() {
        Some(42) => {
            println!("exported file exited with status 42");
            ExitCode::SUCCESS
        }
        code => {
            // A signal (SIGKILL from the kernel for a bad signature, SIGSEGV, SIGBUS) has no code.
            use std::os::unix::process::ExitStatusExt as _;
            eprintln!("export_smoke: the exported file ended with code {code:?}, signal {:?}; wanted status 42", status.signal());
            ExitCode::FAILURE
        }
    }
}
