"""An Ubuntu 22.04 (jammy, glibc 2.35) sysroot for a Linux target, and the C and C++ drivers'
words that build against it, so that what blaze builds on an ix runner (Ubuntu 24.04, glibc
2.39) asks for no newer glibc than a user's machine has: release.yaml's and pgo.yaml's Linux
builds and install.yaml's archive use it, and build_release.py refuses a binary asking for a
glibc newer than RHEL 9's 2.34. Hosted Ubuntu 22.04 gave the same glibc until blaze's kernel
floor (Linux 7.2, docs/kernel.md) moved every job that runs blaze off GitHub's hosted runners,
whose kernels are 6.8 to 6.11 (docs/harness.md "ix runners").

One typed input (rule scripts-take-typed-input):

  uv run --frozen --no-dev python .github/scripts/sysroot.py \\
      '{"target": "aarch64-unknown-linux-gnu", "dir": "/usr/local/lib/blaze-sysroot/aarch64-unknown-linux-gnu",
        "github_env": "'"$GITHUB_ENV"'"}'

It:

  1. downloads the .debs .github/scripts/sysroot.json pins for the target (libc6, libc6-dev,
     linux-libc-dev, libgcc-s1, libstdc++6, libstdc++-11-dev; no libcrypt: no libc6-dev header
     includes crypt.h), all at once, each checked against its pinned sha256. The URLs are
     Launchpad's, which keeps every file it ever published, where archive.ubuntu.com's and
     ports.ubuntu.com's pools drop a version once a newer one supersedes it;
  2. unpacks each with `dpkg-deb --fsys-tarfile` (jammy's .debs are zstd, which Python 3.13's
     tarfile does not read) into `dir`, every absolute symlink (`usr/lib/<multiarch>/libm.so ->
     /lib/<multiarch>/libm.so.6`) made relative so it resolves inside `dir`, through tarfile's
     `data` filter, which refuses anything that would land or point outside it;
  3. rewrites glibc's GNU linker scripts (`libc.so`, x86-64's `libm.so`) to name their files by
     file name alone: `GROUP ( /lib/<multiarch>/libc.so.6 ... )` becomes `GROUP ( libc.so.6
     ... )`. blaze's linker takes a script's absolute path as it is, on the machine that links
     (crates/blaze-link/src/target.rs, `follow`), which would be the runner's own glibc, and GNU
     ld prefixes the sysroot to an absolute path in a script inside it, so the script's paths
     made absolute in `dir` would be prefixed twice; a file name is searched for in the library
     directories by both, and the drivers' words below put the sysroot's first. Each name is
     checked to find the very file the absolute path named;
  4. checks the drivers with those words find every glibc file in `dir`: the C runtime's files,
     libc, the loader and libgcc_s by `-print-file-name`, libstdc++.a by the C++ driver's, and
     the first `stdio.h` (and `cstdlib`) on their include paths;
  5. prints the environment that builds against it, and appends it to `github_env` when given:

       CC_<triple with _>        <gcc> --sysroot=<dir> -B<dir>/usr/lib/<multiarch>/ -B<dir>/lib/<multiarch>/
       CXX_<triple with _>       <g++> (the same) -B<dir>/usr/lib/gcc/<multiarch>/11/
       CFLAGS_<triple with _>    --sysroot=<dir>
       CXXFLAGS_<triple with _>  --sysroot=<dir> -nostdinc++ -isystem <jammy's libstdc++ 11 headers>

     `--sysroot` alone is not enough with Ubuntu 24.04's GCC 13: it searches its own directories
     (`/usr/lib/gcc/...`, and a cross GCC's `/usr/<multiarch>/{include,lib}`) before the
     sysroot's, so `-B` puts the sysroot's libraries first, for the driver's `-print-file-name`
     and `-print-search-dirs` (which blaze's linker asks) and for the `-L`s it passes a linker.
     The C++ runtime is jammy's too: GCC 13's libstdc++.a, built against glibc 2.39, needs
     `__isoc23_strtoul` (glibc 2.38, in `eh_alloc.o`, which every C++ program with exceptions
     loads) and `arc4random` (2.36), which glibc 2.35 does not have; jammy's GCC 11 libstdc++ is
     the one hosted Ubuntu 22.04 linked, and the pinned LLVM was compiled against its headers.
     vendor/llvm-sys/build.rs finds libstdc++.a through `CXX_<triple>`'s words.

The C driver is `gcc` for the host's own target and `<multiarch>-gcc` (Ubuntu's cross GCC) for
another; a cross GCC's own glibc (`libc6-dev-<arch>-cross`, in `/usr/<multiarch>/include`) is
searched before any sysroot's headers, and step 4 refuses it: remove that package first.

`dir` is made when missing; one this script made before (it holds `.blaze-sysroot.json`) is
made again from nothing; any other one that is not empty is refused. Every step is bounded, and a
failure stops the script naming the step, the command, its exit status and the tail of what it
printed.
"""

import asyncio
import hashlib
import json
import os
import platform
import re
import shutil
import sys
import tarfile
import tempfile
import urllib.request
from dataclasses import dataclass
from io import BytesIO
from pathlib import Path, PurePosixPath
from typing import Literal, NoReturn

SCRIPT = ".github/scripts/sysroot.py"
PINS = Path(__file__).resolve().with_name("sysroot.json")
STAMP = ".blaze-sysroot.json"
TAIL_LINES = 40
Triple = Literal["aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"]
HOSTS: dict[str, Triple] = {"aarch64": "aarch64-unknown-linux-gnu", "x86_64": "x86_64-unknown-linux-gnu"}


@dataclass(frozen=True)
class Sysroot:
    # The Rust target triple the sysroot is for.
    target: Triple
    # Where the tree goes: below /usr/local/lib, since a build script in blaze's sandbox may name
    # only system directories of libraries (crates/blaze-cargo/src/directives.rs, SYSTEM), and
    # llvm-sys names libstdc++.a's.
    dir: str
    # The C and C++ drivers; empty: `gcc` and `g++` for the host's target, `<multiarch>-gcc` and
    # `<multiarch>-g++` for another.
    cc: str = ""
    cxx: str = ""
    # GitHub's environment file ($GITHUB_ENV), which the variables are appended to; empty: only
    # printed.
    github_env: str = ""
    download_secs: int = 300
    unpack_secs: int = 120
    check_secs: int = 60


@dataclass(frozen=True)
class Deb:
    package: str
    version: str
    url: str
    sha256: str


@dataclass(frozen=True)
class Target:
    target: str
    # Debian's name of the architecture's library directories (`lib/<multiarch>`).
    multiarch: str
    # glibc's loader's file name.
    loader: str
    debs: tuple[Deb, ...]


@dataclass(frozen=True)
class Pins:
    # The suite the versions are the newest of when pinned.
    release: str
    glibc: str
    # The GCC whose libstdc++ the sysroot holds (`usr/lib/gcc/<multiarch>/<libstdcxx>`).
    libstdcxx: str
    targets: tuple[Target, ...]


class Failed(Exception):
    """A step that did not do what it must, naming it."""


def fail(message: str) -> NoReturn:
    sys.exit(f"{SCRIPT}: {message}")


@dataclass(frozen=True)
class Ran:
    stdout: bytes
    stderr: str


def tail(text: str) -> str:
    return "\n".join(text.splitlines()[-TAIL_LINES:])


async def run(step: str, cmd: list[str], *, secs: float) -> Ran:
    """`cmd` run to its end within `secs`; a nonzero exit or the bound running out is a `Failed`
    naming the step, the command, its status and the tail of its standard error."""
    shown = " ".join(cmd)
    try:
        proc = await asyncio.create_subprocess_exec(
            *cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
        )
    except OSError as e:
        raise Failed(f"{step}: `{shown}` did not start: {e}") from None
    try:
        async with asyncio.timeout(secs):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise Failed(f"{step}: `{shown}` did not end in {secs:.0f} s") from None
    said = err.decode(errors="replace")
    if proc.returncode != 0:
        raise Failed(f"{step}: `{shown}` exited {proc.returncode}; its standard error ends:\n{tail(said)}")
    return Ran(out, said)


def host() -> Triple:
    if sys.platform != "linux":
        raise Failed(f"host: a sysroot is made on Linux, where Debian's dpkg-deb is; this is {sys.platform}")
    found = HOSTS.get(platform.machine())
    if found is None:
        raise Failed(f"host: no sysroot is made on {platform.machine()} (only on {', '.join(sorted(HOSTS))})")
    return found


def pinned(target: str) -> tuple[Pins, Target]:
    import typed_input

    pins = typed_input.parse(Pins, str(PINS), SCRIPT)
    for it in pins.targets:
        if it.target == target:
            return pins, it
    raise Failed(
        f"pins: {PINS} pins no sysroot for {target} (it pins {', '.join(t.target for t in pins.targets)})"
    )


async def download(deb: Deb, into: Path, secs: int) -> Path:
    """`deb` downloaded into `into`, checked against its pinned sha256."""
    path = into / PurePosixPath(deb.url).name

    def fetch() -> str:
        digest = hashlib.sha256()
        with urllib.request.urlopen(deb.url, timeout=secs) as got, open(path, "wb") as f:
            while chunk := got.read(1 << 20):
                digest.update(chunk)
                f.write(chunk)
        return digest.hexdigest()

    try:
        async with asyncio.timeout(secs):
            got = await asyncio.to_thread(fetch)
    except (TimeoutError, OSError) as e:
        raise Failed(f"download {deb.package}: {deb.url}: {e!r}") from None
    if got != deb.sha256:
        raise Failed(
            f"download {deb.package} {deb.version}: {deb.url} has sha256 {got}, not the {deb.sha256} {PINS} pins"
        )
    return path


def relative_links(tar: tarfile.TarFile) -> list[tarfile.TarInfo]:
    """The members of `tar`, each symlink to an absolute path made relative to its own directory,
    so it points inside the sysroot rather than at the machine's own files."""
    members: list[tarfile.TarInfo] = []
    for member in tar.getmembers():
        if member.issym() and member.linkname.startswith("/"):
            at = PurePosixPath(member.name.removeprefix("./")).parent
            member.linkname = os.path.relpath(member.linkname.lstrip("/"), at.as_posix() or ".")
        members.append(member)
    return members


async def unpack(deb: Deb, path: Path, root: Path, secs: int) -> None:
    ran = await run(f"unpack {deb.package}", ["dpkg-deb", "--fsys-tarfile", str(path)], secs=secs)
    try:
        with tarfile.open(fileobj=BytesIO(ran.stdout), mode="r:") as tar:
            tar.extractall(root, members=relative_links(tar), filter="data")
    except (tarfile.TarError, OSError) as e:
        raise Failed(f"unpack {deb.package}: {path.name}: {e}") from None


# A linker script's comments, and an absolute path among its words.
COMMENT = re.compile(r"/\*.*?\*/", re.DOTALL)
ABSOLUTE = re.compile(r"(?<![^\s(])/[^\s()]+")


def library_dirs(root: Path, multiarch: str) -> list[Path]:
    """The sysroot's library directories, in the order the drivers' `-B`s search them."""
    return [root / "usr" / "lib" / multiarch, root / "lib" / multiarch]


def file_name(script: str, absolute: str, root: Path, dirs: list[Path]) -> str:
    """The file name the linker script `script` names `absolute` by: the file the drivers find
    first by that name in `dirs` must be the one `absolute` is in the sysroot."""
    inside = root / absolute.lstrip("/")
    if not inside.is_file():
        raise Failed(f"scripts: /{script} names {absolute}, which the sysroot has no file at")
    file = PurePosixPath(absolute).name
    first = next((d / file for d in dirs if (d / file).exists()), None)
    if first is None or not first.samefile(inside):
        raise Failed(
            f"scripts: /{script} names {absolute}, but the file named {file} found first in "
            f"{', '.join(map(str, dirs))} is {first}, another file"
        )
    return file


def rewrite_scripts(root: Path, multiarch: str) -> list[str]:
    """Every GNU linker script among the sysroot's libraries, its absolute paths made file names
    (see the top of the file); what each became."""
    dirs = library_dirs(root, multiarch)
    said: list[str] = []
    for d in dirs:
        for path in sorted(d.glob("*.so")):
            if path.is_symlink() or not path.is_file():
                continue
            data = path.read_bytes()
            if data[:4] == b"\x7fELF":
                continue
            text = data.decode()
            if "GROUP" not in text and "INPUT" not in text:
                raise Failed(f"scripts: {path} is neither ELF nor a GNU linker script naming GROUP or INPUT")
            name = str(path.relative_to(root))

            def by_name(found: re.Match[str], name: str = name) -> str:
                return file_name(name, found.group(0), root, dirs)

            parts = COMMENT.split(text)
            comments = COMMENT.findall(text)
            rewritten = ABSOLUTE.sub(by_name, parts[0])
            for comment, part in zip(comments, parts[1:], strict=True):
                rewritten += comment + ABSOLUTE.sub(by_name, part)
            if rewritten != text:
                path.write_text(rewritten)
                body = " ".join(COMMENT.sub("", rewritten).split())
                said.append(f"/{name}: {body}")
    return said


def words(pins: Pins, target: Target, root: Path, cc: str, cxx: str) -> dict[str, str]:
    """The environment that builds `target` against the sysroot at `root`."""
    ma = target.multiarch
    b = " ".join(f"-B{d}/" for d in library_dirs(root, ma))
    gcc_dir = root / "usr" / "lib" / "gcc" / ma / pins.libstdcxx
    headers = root / "usr" / "include" / "c++" / pins.libstdcxx
    cxx_includes = [headers, root / "usr" / "include" / ma / "c++" / pins.libstdcxx, headers / "backward"]
    var = target.target.replace("-", "_")
    return {
        f"CC_{var}": f"{cc} --sysroot={root} {b}",
        f"CXX_{var}": f"{cxx} --sysroot={root} {b} -B{gcc_dir}/",
        f"CFLAGS_{var}": f"--sysroot={root}",
        f"CXXFLAGS_{var}": f"--sysroot={root} -nostdinc++ " + " ".join(f"-isystem {d}" for d in cxx_includes),
    }


def inside(path: Path, root: Path) -> bool:
    return path.resolve().is_relative_to(root.resolve())


async def print_file(step: str, driver: list[str], file: str, root: Path, secs: int) -> str:
    """Where `driver -print-file-name=file` finds `file`, refused unless in the sysroot."""
    ran = await run(step, [*driver, f"-print-file-name={file}"], secs=secs)
    found = Path(ran.stdout.decode().strip())
    if not found.is_absolute() or not found.exists():
        raise Failed(
            f"{step}: `{' '.join(driver)} -print-file-name={file}` found no {file} (it said {found})"
        )
    if not inside(found, root):
        raise Failed(
            f"{step}: `{' '.join(driver)} -print-file-name={file}` found {found}, outside the sysroot {root}: "
            "a build would link the runner's own"
        )
    return f"{file}: {found}"


async def first_header(step: str, driver: list[str], lang: str, header: str, root: Path, secs: int) -> str:
    """The include directory where `driver` finds `header` first, refused unless in the sysroot."""
    ran = await run(step, [*driver, "-E", "-v", "-x", lang, os.devnull], secs=secs)
    lines = ran.stderr.splitlines()
    try:
        start = next(
            i for i, line in enumerate(lines) if line.startswith("#include <...> search starts here:")
        )
        end = next(i for i, line in enumerate(lines) if line.startswith("End of search list."))
    except StopIteration:
        raise Failed(
            f"{step}: `{' '.join(driver)} -E -v` printed no include search list:\n{tail(ran.stderr)}"
        ) from None
    dirs = [Path(line.strip()) for line in lines[start + 1 : end]]
    found = next((d for d in dirs if (d / header).exists()), None)
    if found is None:
        raise Failed(
            f"{step}: `{' '.join(driver)}` finds no {header} on its include path {[str(d) for d in dirs]}"
        )
    if not inside(found, root):
        raise Failed(
            f"{step}: `{' '.join(driver)}` finds {header} first in {found}, outside the sysroot {root} (its include "
            f"path: {', '.join(map(str, dirs))}); a cross GCC's own glibc (libc6-dev-<arch>-cross) is searched before "
            "the sysroot: remove it"
        )
    return f"{lang} {header}: {found}"


async def check(target: Target, env: dict[str, str], root: Path, secs: int) -> list[str]:
    var = target.target.replace("-", "_")
    cc = env[f"CC_{var}"].split() + env[f"CFLAGS_{var}"].split()
    cxx = env[f"CXX_{var}"].split() + env[f"CXXFLAGS_{var}"].split()
    files = ["Scrt1.o", "crti.o", "crtn.o", "libc.so.6", "libc.so", "libc_nonshared.a", target.loader]
    files += ["libm.so", "libpthread.a", "libdl.a", "libgcc_s.so.1"]
    async with asyncio.TaskGroup() as group:
        tasks = [group.create_task(print_file(f"check {f}", cc, f, root, secs)) for f in files]
        tasks.append(group.create_task(print_file("check libstdc++.a", cxx, "libstdc++.a", root, secs)))
        tasks.append(group.create_task(first_header("check C headers", cc, "c", "stdio.h", root, secs)))
        for header in ("cstdlib", "stdio.h"):
            tasks.append(
                group.create_task(first_header(f"check C++ {header}", cxx, "c++", header, root, secs))
            )
    return [t.result() for t in tasks]


def prepare(root: Path) -> None:
    """`root` empty and ready: made when missing, a sysroot of this script's made again from
    nothing, anything else that is not empty refused."""
    if root.is_dir() and any(root.iterdir()):
        if not (root / STAMP).is_file():
            raise Failed(
                f"dir: {root} is not empty and is no sysroot of {SCRIPT} (it has no {STAMP}): not replaced"
            )
        shutil.rmtree(root)
    try:
        root.mkdir(parents=True, exist_ok=True)
    except OSError as e:
        raise Failed(
            f"dir: {root}: {e} (make its parent writable first, as the workflows do with sudo)"
        ) from None


def append(path: Path, lines: list[str]) -> None:
    with path.open("a") as f:
        f.write("".join(f"{line}\n" for line in lines))


async def main_async(given: Sysroot) -> None:
    on = host()
    pins, target = pinned(given.target)
    native = on == given.target
    cc = given.cc or ("gcc" if native else f"{target.multiarch}-gcc")
    cxx = given.cxx or ("g++" if native else f"{target.multiarch}-g++")
    for program in (cc, cxx, "dpkg-deb"):
        if shutil.which(program.split()[0]) is None:
            raise Failed(f"tools: `{program}` is not on PATH")
    root = Path(given.dir).absolute()
    if not root.is_relative_to("/usr/local/lib"):
        raise Failed(
            f"dir: {root} is not below /usr/local/lib, where a build script in blaze's sandbox may name libraries "
            "(crates/blaze-cargo/src/directives.rs, SYSTEM) and llvm-sys names libstdc++.a's"
        )
    prepare(root)
    # First, so a run that fails part way leaves a tree the next run knows to make again.
    stamp: dict[str, object] = {"release": pins.release, "glibc": pins.glibc, "target": target.target}
    stamp["debs"] = [f"{d.package} {d.version} {d.sha256}" for d in target.debs]
    (root / STAMP).write_text(json.dumps(stamp, indent=2) + "\n")
    with tempfile.TemporaryDirectory(prefix="blaze-sysroot-") as tmp:
        async with asyncio.TaskGroup() as group:
            fetched = [group.create_task(download(d, Path(tmp), given.download_secs)) for d in target.debs]
        for deb, task in zip(target.debs, fetched, strict=True):
            await unpack(deb, task.result(), root, given.unpack_secs)
            print(f"sysroot: {deb.package} {deb.version}", flush=True)
    for line in rewrite_scripts(root, target.multiarch):
        print(f"sysroot: rewrote {line}", flush=True)
    env = words(pins, target, root, cc, cxx)
    for line in await check(target, env, root, given.check_secs):
        print(f"sysroot: found {line}", flush=True)
    lines = [f"{name}={value}" for name, value in env.items()]
    if given.github_env:
        append(Path(given.github_env), lines)
    print(
        f"sysroot: glibc {pins.glibc} ({pins.release}) for {target.target} in {root}"
        + (f"; set in {given.github_env}:" if given.github_env else "; set:"),
        flush=True,
    )
    for line in lines:
        print(line, flush=True)


def main() -> None:
    if len(sys.argv) != 2:
        fail("usage: sysroot.py CONFIG, one JSON object (see the top of the file)")
    import typed_input

    given = typed_input.parse(Sysroot, sys.argv[1], SCRIPT)
    try:
        asyncio.run(main_async(given))
    except Failed as e:
        fail(str(e))
    except ExceptionGroup as group:
        failed, rest = group.split(Failed)
        if rest is not None or failed is None:
            raise
        fail("; ".join(str(e) for e in failed.exceptions))


if __name__ == "__main__":
    main()
