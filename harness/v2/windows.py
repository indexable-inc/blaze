"""windows.py: observable Windows x86-64 MSVC regression coverage.

    uv run --frozen python harness/v2/windows.py '{"engine":{"kind":"v2","blaze":"target/release/blaze.exe"},"out":"windows.json"}'
    uv run --frozen python harness/v2/windows.py selftest

The input is one typed JSON object. ``engine`` and ``out`` are the usual v2 harness
fields; ``work`` names the directory to use, ``keep`` retains it, and ``scenarios``
selects from paths, linking and daemon (empty means all).

This is intentionally an end-to-end platform harness.  Path spellings, PE imports,
executed programs and daemon process lifetime are facts only Windows can expose. Incremental
state histories belong in the query engine's deterministic simulation, not this native driver.
"""

import ctypes
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
import engine
import report
import storelib
from typed_input import fail, parse

SCRIPT = "windows.py"
SCENARIOS = ("paths", "linking", "daemon")
BUILD_SECS = 1800
RUN_SECS = 120


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    scenarios: tuple[str, ...] = ()
    keep: bool = False


class Regression(Exception):
    """An observable result differs from the Windows rule under test."""


def manifest(name: str, *, dependency: str = "", crate_type: str = "") -> str:
    dep = f'\n[dependencies]\nwin-dep = {{ path = "{dependency}" }}\n' if dependency else ""
    lib = f'\n[lib]\ncrate-type = ["{crate_type}"]\n' if crate_type else ""
    return (
        f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2024"\npublish = false\n'
        f"{lib}{dep}\n[workspace]\n"
    )


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8", newline="\n")


def executable(eng: engine.Engine, ws: Path, env: dict[str, str], name: str, profile: str = "debug") -> Path:
    return eng.build_dir(ws, env) / profile / f"{name}{engine.EXE}"


def build(
    eng: engine.Engine, label: str, ws: Path, env: dict[str, str], *, release: bool = False
) -> engine.Ran:
    args = ["--message-format=json", *(["--release"] if release else [])]
    ran = eng.build(label, ws, env, args, timeout=BUILD_SECS)
    if ran.returncode != 0:
        raise Regression(f"{label}: blaze exited {ran.returncode}:\n{ran.said()}")
    if who := eng.not_blazes(ran):
        raise Regression(f"{label}: the build was not blaze's: {who}")
    return ran


def run_program(path: Path, cwd: Path) -> str:
    ran = engine.run(f"running {path.name}", [str(path)], cwd, engine.scrubbed(), RUN_SECS)
    if ran.returncode != 0:
        raise Regression(f"{path} exits {ran.returncode}:\n{ran.said()}")
    return ran.stdout.replace("\r\n", "\n")


def expect(label: str, got: str, wanted: str) -> None:
    if got != wanted:
        raise Regression(f"{label}: expected {wanted!r}, got {got!r}")


def path_spellings(path: Path) -> tuple[str, ...]:
    """The native, slash, case-varied and Win32 verbatim spellings of one path."""
    native = str(path.resolve())
    drive, tail = os.path.splitdrive(native)
    case = drive.swapcase() + tail.swapcase()
    # Keep these as strings until subprocess's cwd boundary: pathlib normalizes the slash form.
    return (native, native.replace("\\", "/"), case, "\\\\?\\" + native)


def paths(eng: engine.Engine, top: Path, rep: report.Report) -> None:
    root = top / "Paths with spaces Ω"
    registry = root / "Cargo Registry" / "registry" / "src" / "example.invalid-000000" / "win-dep-0.1.0"
    write(registry / "Cargo.toml", manifest("win-dep"))
    write(registry / "src/lib.rs", 'pub fn value() -> &\'static str { "registry Ω" }\n')
    ws = root / "Workspace MixedCase"
    relative = os.path.relpath(registry, ws).replace("\\", "/")
    write(ws / "Cargo.toml", manifest("windows_paths", dependency=relative))
    write(ws / "src/main.rs", 'fn main() { println!("{}", win_dep::value()); }\n')
    home, target = top / "path-home", top / "path-target"
    env = eng.env(home, target)
    for index, spelling in enumerate(path_spellings(ws)):
        build(eng, f"windows paths: spelling {index + 1}", spelling, env)  # type: ignore[arg-type]
        expect(
            f"path spelling {spelling}",
            run_program(executable(eng, Path(spelling), env, "windows_paths"), top),
            "registry Ω\n",
        )
    rep.extra["path_spellings"] = [str(p) for p in path_spellings(ws)]
    rep.add("windows/paths", "pass", native=str(ws))


RAW_DLL = r"""#[link(name = "kernel32.dll", kind = "raw-dylib", modifiers = "+verbatim")]
unsafe extern "system" { fn GetCurrentProcessId() -> u32; }
#[link(name = "user32")]
unsafe extern "system" { fn GetSystemMetrics(index: i32) -> i32; }
fn main() {
    println!("pid-positive={}", unsafe { GetCurrentProcessId() } > 0);
    println!("sdk-library-called={}", unsafe { GetSystemMetrics(0) } >= 0);
}
"""


def dumpbin(path: Path) -> str:
    found = shutil.which("dumpbin.exe") or shutil.which("dumpbin")
    if not found:
        raise engine.Failed(
            "MSVC linking: no dumpbin.exe on PATH; the Windows SDK/MSVC environment is incomplete"
        )
    ran = engine.run(
        f"PE imports of {path.name}",
        [found, "/dependents", str(path)],
        path.parent,
        engine.scrubbed(),
        RUN_SECS,
    )
    if ran.returncode != 0:
        raise engine.Failed(f"dumpbin /dependents {path} exits {ran.returncode}:\n{ran.said()}")
    return ran.stdout.lower()


def linking(eng: engine.Engine, top: Path, rep: report.Report) -> None:
    raw = top / "raw dll verbatim"
    write(raw / "Cargo.toml", manifest("raw_dll_verbatim"))
    write(raw / "src/main.rs", RAW_DLL)
    raw_env = eng.env(top / "link-home", top / "raw-target")
    build(eng, "raw DLL import with +verbatim", raw, raw_env)
    expect(
        "raw DLL program",
        run_program(executable(eng, raw, raw_env, "raw_dll_verbatim"), top),
        "pid-positive=true\nsdk-library-called=true\n",
    )

    imports: dict[str, str] = {}
    for mode, feature in (("dynamic", "-crt-static"), ("static", "+crt-static")):
        ws = top / f"crt-{mode}"
        write(ws / "Cargo.toml", manifest(f"crt_{mode}"))
        write(
            ws / ".cargo/config.toml",
            f'[target.x86_64-pc-windows-msvc]\nrustflags = ["-C", "target-feature={feature}"]\n',
        )
        write(ws / "src/main.rs", f'fn main() {{ println!("crt={mode}"); }}\n')
        env = eng.env(top / f"crt-{mode}-home", top / f"crt-{mode}-target")
        build(eng, f"{mode} CRT", ws, env, release=True)
        program = executable(eng, ws, env, f"crt_{mode}", "release")
        expect(f"{mode} CRT program", run_program(program, top), f"crt={mode}\n")
        imports[mode] = dumpbin(program)
    crt_dlls = ("ucrtbase.dll", "vcruntime140.dll", "msvcp140.dll")
    if not any(name in imports["dynamic"] for name in crt_dlls):
        raise Regression("dynamic CRT executable imports none of " + ", ".join(crt_dlls))
    if any(name in imports["static"] for name in crt_dlls):
        raise Regression("static CRT executable still imports a dynamic CRT:\n" + imports["static"][-3000:])

    dll = top / "dll naming"
    write(dll / "Cargo.toml", manifest("windows_named", crate_type="cdylib"))
    write(
        dll / "src/lib.rs", '#[unsafe(no_mangle)]\npub extern "C" fn blaze_windows_answer() -> u32 { 42 }\n'
    )
    dll_env = eng.env(top / "dll-home", top / "dll-target")
    build(eng, "Windows DLL and import library naming", dll, dll_env, release=True)
    directory = eng.build_dir(dll, dll_env) / "release"
    library, import_library = directory / "windows_named.dll", directory / "windows_named.dll.lib"
    if not library.is_file() or not import_library.is_file():
        raise Regression(f"cdylib outputs are not windows_named.dll and windows_named.dll.lib in {directory}")
    loaded = ctypes.WinDLL(str(library))
    answer = loaded.blaze_windows_answer
    answer.restype = ctypes.c_uint32
    if answer() != 42:
        raise Regression(f"{library} blaze_windows_answer returned {answer()}, not 42")
    rep.add(
        "windows/msvc-linking",
        "pass",
        raw_dll=True,
        dynamic_crt=True,
        static_crt=True,
        dll=library.name,
        import_library=import_library.name,
    )


def clean_copy(source: Path, destination: Path) -> None:
    if destination.exists():
        shutil.rmtree(destination)
    shutil.copytree(source, destination, ignore=shutil.ignore_patterns("target"))


def kill_process(pid: int) -> None:
    ran = subprocess.run(
        ["taskkill", "/F", "/T", "/PID", str(pid)], capture_output=True, text=True, timeout=60, check=False
    )
    if ran.returncode != 0:
        raise engine.Failed(
            f"taskkill daemon {pid} exited {ran.returncode}: {(ran.stdout + ran.stderr)[-1500:]}"
        )


def daemon(eng: engine.Engine, top: Path, rep: report.Report) -> None:
    ws = top / "daemon workspace"
    write(ws / "Cargo.toml", manifest("windows_daemon"))
    write(ws / "src/main.rs", 'fn main() { println!("generation=1"); }\n')
    home, target = top / "daemon-home", top / "daemon-target"
    env = eng.env(home, target)
    build(eng, "daemon starts", ws, env)
    first = storelib.blaze_status(str(eng.blaze), env, str(ws)).daemon
    if first is None:
        raise Regression("the first build left no daemon answering")
    eng.stop(ws, env)
    if storelib.blaze_status(str(eng.blaze), env, str(ws)).daemon is not None:
        raise Regression("blaze stop returned while the daemon still answered")

    write(ws / "src/main.rs", 'fn main() { println!("generation=2"); }\n')
    build(eng, "daemon restarts after source change", ws, env)
    second = storelib.blaze_status(str(eng.blaze), env, str(ws)).daemon
    if second is None or second.pid == first.pid:
        raise Regression(f"restart did not start a new daemon (before {first.pid}, after {second})")
    expect(
        "program after restart",
        run_program(executable(eng, ws, env, "windows_daemon"), top),
        "generation=2\n",
    )
    eng.stop(ws, env)
    mark = eng.mark(home)
    build(eng, "cache reused across restart", ws, env)
    reused = eng.work_since(home, mark)
    if not reused.found:
        raise Regression(f"the unchanged build after restart found no stored result: {reused.said}")

    # The negative control completes this exact large build without interruption. Then one source
    # edit starts a new request; only an observed active request authorizes killing its daemon.
    bulky = "\n".join(f"#[inline(never)] fn f{i}() -> usize {{ {i} }}" for i in range(4000))
    calls = " + ".join(f"f{i}()" for i in range(4000))
    write(ws / "src/main.rs", f'{bulky}\nfn main() {{ println!("sum={{}}", {calls}); }}\n')
    build(eng, "uninterrupted-build control", ws, env)
    write(ws / "src/main.rs", f'{bulky}\nfn main() {{ println!("sum={{}}", {calls} + 1); }}\n')
    before = storelib.blaze_status(str(eng.blaze), env, str(ws)).daemon
    before_requests = set(before.requests if before is not None else ())
    client = subprocess.Popen(
        [str(eng.blaze), "build", "--message-format=json"],
        cwd=ws,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    deadline, killed = time.monotonic() + 30, None
    while time.monotonic() < deadline and client.poll() is None:
        status = storelib.blaze_status(str(eng.blaze), env, str(ws), 10).daemon
        if status is not None and set(status.requests) - before_requests:
            killed = status.pid
            kill_process(status.pid)
            break
        time.sleep(0.05)
    try:
        stdout, stderr = client.communicate(timeout=60)
    except subprocess.TimeoutExpired:
        client.kill()
        client.communicate()
        raise engine.Failed("the build client did not end within 60 s after its daemon was killed") from None
    if killed is None:
        raise engine.Failed("the interrupted-build control observed no new active request within 30 s")
    if client.returncode == 0:
        raise Regression(
            "the interrupted-build client succeeded after its active daemon was killed; "
            f"stdout={stdout[-1000:]!r}, stderr={stderr[-1000:]!r}"
        )
    build(eng, "recovery after interrupted build", ws, env)
    recovered = run_program(executable(eng, ws, env, "windows_daemon"), top)
    scratch = top / "daemon-clean"
    clean_copy(ws, scratch)
    clean_env = eng.env(top / "daemon-clean-home", top / "daemon-clean-target")
    build(eng, "clean build after interruption", scratch, clean_env)
    expect(
        "recovered build equals clean build",
        recovered,
        run_program(executable(eng, scratch, clean_env, "windows_daemon"), top),
    )
    rep.add(
        "windows/daemon",
        "pass",
        first_pid=first.pid,
        restarted_pid=second.pid,
        interrupted_pid=killed,
        cache_found=reused.found,
    )


def selected(names: tuple[str, ...]) -> tuple[str, ...]:
    unknown = sorted(set(names) - set(SCENARIOS))
    if unknown:
        raise ValueError(f"scenarios: unknown {', '.join(unknown)}; choose from {', '.join(SCENARIOS)}")
    return names or SCENARIOS


def main(args: Input) -> None:
    if sys.platform != "win32" or platform.machine().lower() not in ("amd64", "x86_64"):
        fail(SCRIPT, f"requires Windows x86-64 MSVC, this host is {sys.platform} {platform.machine()}")
    try:
        scenarios = selected(args.scenarios)
    except ValueError as error:
        fail(SCRIPT, str(error))
    try:
        eng = engine.Engine.of(args.engine)
        # storelib.work_dir probes Linux FICLONE and macOS clonefile. Windows placement is
        # exercised by this harness itself and has no corresponding probe there.
        top = Path(os.path.realpath(args.work)) if args.work else Path(tempfile.mkdtemp(prefix="bz-windows-"))
    except (engine.Failed, storelib.Failed) as error:
        fail(SCRIPT, str(error))
    top.mkdir(parents=True, exist_ok=True)
    rep = report.Report(SCRIPT, args.engine, args.out)
    rep.extra.update(work=str(top), scenarios=list(scenarios), toolchain=engine.TOOLCHAIN)
    functions = {"paths": paths, "linking": linking, "daemon": daemon}
    try:
        for name in scenarios:
            try:
                functions[name](eng, top / name, rep)
            except Regression as error:
                rep.add(f"windows/{name}", "fail", str(error))
            except (engine.Failed, storelib.Failed, OSError, subprocess.SubprocessError) as error:
                rep.add(f"windows/{name}", "error", f"{type(error).__name__}: {error}")
    finally:
        homes = [
            path
            for path in top.rglob("*")
            if path.is_dir() and (path.name == "home" or path.name.endswith("-home"))
        ]
        for home in homes:
            if home.is_dir():
                try:
                    eng.stop(top, eng.env(home))
                except (engine.Failed, OSError):
                    pass
        if not args.keep:
            shutil.rmtree(top, ignore_errors=True)
    rep.finish()


def selftest() -> int:
    tests = (
        lambda: expect("line endings", "x\n", "x\n"),
        lambda: selected(()) == SCENARIOS or (_ for _ in ()).throw(AssertionError()),
        lambda: (
            selected(("paths", "daemon")) == ("paths", "daemon") or (_ for _ in ()).throw(AssertionError())
        ),
        lambda: 'kind = "raw-dylib"' in RAW_DLL or (_ for _ in ()).throw(AssertionError()),
        lambda: path_spellings(Path("C:/Mixed Path"))[1].count("/") >= 2
        or (_ for _ in ()).throw(AssertionError()),
    )
    try:
        selected(("not-a-scenario",))
    except ValueError as error:
        assert "not-a-scenario" in str(error)
    else:
        raise AssertionError("an unknown scenario was accepted")
    for test in tests:
        test()
    print(f"{SCRIPT}: {len(tests) + 1} selftests passed")
    return 0


if __name__ == "__main__":
    if sys.argv[1:] == ["selftest"]:
        raise SystemExit(selftest())
    if len(sys.argv) != 2:
        fail(SCRIPT, f"usage: {SCRIPT} JSON | selftest")
    main(parse(Input, sys.argv[1], SCRIPT))
