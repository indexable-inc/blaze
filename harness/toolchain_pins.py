#!/usr/bin/env python3
"""toolchain_pins.py: the one Python reader of the repository's pins/ folder (pins/README.md),
and the writer of pins/rust.txt: the checksums of the Rust nightly blaze downloads as its own
toolchain, read from the nightly's channel manifest on static.rust-lang.org. blaze refuses a
download whose bytes hash to anything else. rust.txt also has what that rustc and cargo say their
versions are and the commit rustc was built from; pins/toolchain (`nightly-YYYY-MM-DD`) is the one
pin it is written from.

    toolchain_pins.py '{}'                  say whether rust.txt is what the manifest says
    toolchain_pins.py '{"write": true}'     write rust.txt, after pins/toolchain moves

The JSON is inline, a file's path, or `-` for stdin.

The manifest itself is checked against the `.sha256` the Rust project publishes beside it, and
its own hash is pinned too, since blaze writes it into the toolchain as rustup does.

As a module (every Python script that needs a pin imports it, so no script reads a pin's file
itself or a file inside a crate):

    sys.path.insert(0, str(ROOT / "harness"))
    import toolchain_pins as pins
    pins.toolchain()        # "nightly-2026-10-27"
    pins.rust()["commit"]   # rust.txt's fields: release, date, channel, rustc, cargo, commit
    pins.llvm(), pins.clang(), pins.answers()   # the JSON tables, parsed

Rust reads the same files at compile time through crates/blaze-pins.
"""
import hashlib, json, os, sys, tomllib, urllib.request
from dataclasses import dataclass, fields

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
DIR = os.path.join(ROOT, "pins")
# Each pin's path; crates/blaze-pins names the same files (`TOOLCHAIN_PATH`, `RUST_PATH`, ...).
TOOLCHAIN = os.path.join(DIR, "toolchain")
RUST = os.path.join(DIR, "rust.txt")
LLVM = os.path.join(DIR, "llvm.json")
CLANG = os.path.join(DIR, "clang.json")
ANSWERS = os.path.join(DIR, "answers", "rustc.json")
OWN_TARGETS = os.path.join(DIR, "answers", "own_targets.json")
DIST = "https://static.rust-lang.org/dist"
# The hosts blaze has a build for, and Windows for `cargo install`.
HOSTS = ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
         "x86_64-pc-windows-msvc"]
# rustup's `minimal` profile without rust-docs, and wasm32-wasip1's standard library for the C
# runtime (wasi-libc, `lib/rustlib/wasm32-wasip1/lib/self-contained`) that blaze's linker links
# every proc macro's module with (blaze-link `target.rs`, `wasi`): blaze's own copy is the only
# one it reads, never rustup's (docs/v2/plan/design/toolchain-c.md, slice 2). `None` is the host. The
# standard library's source is not downloaded: it is blaze's own library/ (docs/toolchain.md).
COMPONENTS = [("rustc", None), ("cargo", None), ("rust-std", None), ("rust-std", "wasm32-wasip1")]
# rust.txt's lines of one field each; every other line is an archive row.
KEYS = ("release", "rustc", "cargo", "commit", "manifest")


def rel(path):
    return os.path.relpath(path, ROOT)


def toolchain():
    """The pinned nightly, `nightly-YYYY-MM-DD`, from pins/toolchain; exits naming the file when
    it is of another form."""
    try:
        text = open(TOOLCHAIN).read()
    except OSError as e:
        sys.exit(f"{rel(TOOLCHAIN)}: {e}")
    name = text.strip()
    channel, _, date = name.partition("-")
    if text != name + "\n" or channel != "nightly" or len(date) != 10:
        sys.exit(f"{rel(TOOLCHAIN)}:1: is `{text!r}`, not one `nightly-YYYY-MM-DD` line")
    return name


def rust():
    """pins/rust.txt's fields: `release` (the toolchain), `date`, `channel`, `rustc` and `cargo`
    (what `-V` prints after the program's name), `commit`, `manifest` ((sha256, bytes)) and
    `archives` ([(host, package, target, sha256, bytes)]). Exits naming the line of a malformed
    one, as blaze-pins stops the build."""
    out = {"archives": []}
    try:
        lines = open(RUST).read().split("\n")
    except OSError as e:
        sys.exit(f"{rel(RUST)}: {e}")
    for number, line in enumerate(lines, 1):
        if not line or line.startswith("#"):
            continue
        key, _, rest = line.partition(" ")
        words = rest.split()
        if key in KEYS and key in out:
            sys.exit(f"{rel(RUST)}:{number}: a second `{key}` line")
        if key == "release":
            if len(words) != 3:
                sys.exit(f"{rel(RUST)}:{number}: `release` takes <toolchain> <date> <channel>")
            out["release"], out["date"], out["channel"] = words
        elif key in ("rustc", "cargo"):
            out[key] = rest
        elif key == "commit":
            out["commit"] = rest
        elif key == "manifest":
            if len(words) != 2 or not words[1].isdigit():
                sys.exit(f"{rel(RUST)}:{number}: `manifest` takes <sha256> <bytes>")
            out["manifest"] = (words[0], int(words[1]))
        else:
            row = line.split()
            if len(row) != 5 or not row[4].isdigit():
                sys.exit(f"{rel(RUST)}:{number}: an archive row is <host> <package> <target> <sha256> <bytes>")
            out["archives"].append((*row[:4], int(row[4])))
    for key in KEYS:
        if key not in out:
            sys.exit(f"{rel(RUST)}: no `{key}` line")
    return out


def _json(path):
    try:
        return json.load(open(path))
    except (OSError, json.JSONDecodeError) as e:
        sys.exit(f"{rel(path)}: {e}")


def llvm():
    """pins/llvm.json, parsed: the LLVM blaze links."""
    return _json(LLVM)


def clang():
    """pins/clang.json, parsed: the C toolchain native build scripts run with."""
    return _json(CLANG)


def answers():
    """pins/answers/rustc.json, parsed: what the pinned rustc answers, per target."""
    return _json(ANSWERS)


def get(url):
    with urllib.request.urlopen(url) as answer:
        return answer.read()


def size(url):
    with urllib.request.urlopen(urllib.request.Request(url, method="HEAD")) as answer:
        return int(answer.headers["Content-Length"])


def manifest_pins():
    """rust.txt's text, from the channel manifest of the nightly pins/toolchain names."""
    release = toolchain()
    # A dated nightly: its manifest and files are under the date, named for the channel.
    channel_name, _, wanted = release.partition("-")
    manifest = get(f"{DIST}/{wanted}/channel-rust-{channel_name}.toml")
    published = get(f"{DIST}/{wanted}/channel-rust-{channel_name}.toml.sha256").split()[0].decode()
    digest = hashlib.sha256(manifest).hexdigest()
    if digest != published:
        sys.exit(f"the channel manifest hashes to {digest}, and its .sha256 says {published}")
    channel = tomllib.loads(manifest.decode())
    date = channel["date"]
    if date != wanted:
        sys.exit(f"the manifest of {release} is dated {date}")
    rustc = channel["pkg"]["rustc"]["version"]
    commit = channel["pkg"]["rust"]["git_commit_hash"]
    # `cargo -V` prints rustc's release (cargo is built with the tree's CFG_RELEASE) and its own
    # commit and date, which the manifest gives after its crate version: `0.N.0-nightly (<hash>
    # <date>)`. Build scripts ask it (codspeed); blaze answers it from this line.
    cargo = channel["pkg"]["cargo"]["version"]
    if "(" not in cargo:
        sys.exit(f"the manifest's cargo version `{cargo}` names no commit")
    cargo = f"{rustc.split()[0]} {cargo[cargo.index('('):]}"
    lines = [
        f"# The Rust nightly blaze downloads as its own toolchain: written by",
        f"# harness/toolchain_pins.py '{{\"write\": true}}' from {DIST}/{date}/channel-rust-{channel_name}.toml.",
        f"# release <toolchain> <date> <channel>; rustc <what `rustc -V` says after `rustc `>;",
        f"# cargo <what `cargo -V` says after `cargo `>;",
        f"# commit <rust-lang/rust commit>; manifest <sha256> <bytes>; then per host",
        f"# <host> <package> <target> <sha256 of the .tar.xz> <bytes>.",
        f"release {release} {date} {channel_name}",
        f"rustc {rustc}",
        f"cargo {cargo}",
        f"commit {commit}",
        f"manifest {digest} {len(manifest)}",
    ]
    for host in HOSTS:
        for package, target in COMPONENTS:
            target = target or host
            built = channel["pkg"][package]["target"][target]
            if not built.get("available"):
                sys.exit(f"{package} is not built for {target} in Rust {release}")
            url = built["xz_url"]
            want = f"{DIST}/{date}/{package}-{channel_name}-{target}.tar.xz"
            if url != want:
                sys.exit(f"{package} for {target} is at {url}, where blaze looks for {want}")
            lines.append(f"{host} {package} {target} {built['xz_hash']} {size(url)}")
    return "\n".join(lines) + "\n"


@dataclass(frozen=True)
class Pins:
    write: bool = False


def typed(text):
    """[`Pins`] from the JSON `text` (inline, a path, or `-`); exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif text and not text.lstrip().startswith("{"):
        text = open(text).read()
    try:
        value = json.loads(text or "{}")
    except json.JSONDecodeError as e:
        sys.exit(f"toolchain_pins.py: the input is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("toolchain_pins.py: the input is a JSON object")
    if unknown := sorted(set(value) - {f.name for f in fields(Pins)}):
        sys.exit(f"toolchain_pins.py: no field {', '.join(unknown)} (fields: write)")
    if not isinstance(value.get("write", False), bool):
        sys.exit(f"toolchain_pins.py: field write is a bool, not {value['write']!r}")
    return Pins(**value)


def main():
    if len(sys.argv) > 2:
        sys.exit(f"usage: toolchain_pins.py '<json>'\n\n{__doc__}")
    asked = typed(sys.argv[1] if len(sys.argv) == 2 else "")
    written = manifest_pins()
    release = toolchain()
    if asked.write:
        with open(RUST, "w") as out:
            out.write(written)
        print(f"wrote {rel(RUST)} for Rust {release}")
        return
    have = open(RUST).read() if os.path.exists(RUST) else ""
    if have != written:
        sys.exit(f"{rel(RUST)} is not what Rust {release}'s manifest says: "
                 "toolchain_pins.py '{\"write\": true}' writes it")
    print(f"{rel(RUST)} is Rust {release}'s manifest")


if __name__ == "__main__":
    main()
