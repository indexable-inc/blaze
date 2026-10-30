#!/usr/bin/env python3
"""c_toolchain_pins.py: the C toolchain blaze compiles build scripts' C with for the wasm32 targets
(crates/blaze-cargo/src/cargo/toolchain/c_pins.txt, docs/c-toolchain.md): the wasi-sdk release the
pinned nightly builds its wasm32 wasi-libc with, so the headers a C file is compiled against are
those of the libc.a the module links, and the sha256 and size GitHub publishes for each host's
archive. blaze refuses a download whose bytes hash to anything else.

    c_toolchain_pins.py '{}'                  say whether c_pins.txt is what the nightly and GitHub say
    c_toolchain_pins.py '{"write": true}'     write c_pins.txt, after the nightly moves

The JSON is inline, a file's path, or `-` for stdin. The nightly is the commit
crates/blaze-cargo/src/cargo/toolchain/pins.txt names (harness/toolchain_pins.py).
"""
import json, os, re, sys, urllib.request
from dataclasses import dataclass, fields

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RUST_PINS = os.path.join(ROOT, "crates/blaze-cargo/src/cargo/toolchain/pins.txt")
PINS = os.path.join(ROOT, "crates/blaze-cargo/src/cargo/toolchain/c_pins.txt")
# Where the nightly says which wasi-sdk its wasm32 standard libraries are built with.
DOCKERFILE = "src/ci/docker/host-x86_64/dist-various-2/Dockerfile"
RELEASES = "https://github.com/WebAssembly/wasi-sdk/releases/download"
# The hosts blaze has a build for (as harness/toolchain_pins.py), by wasi-sdk's names for them.
HOSTS = {
    "aarch64-apple-darwin": "arm64-macos",
    "x86_64-unknown-linux-gnu": "x86_64-linux",
    "aarch64-unknown-linux-gnu": "arm64-linux",
    "x86_64-pc-windows-msvc": "x86_64-windows",
}


def get(url):
    request = urllib.request.Request(url, headers={"User-Agent": "blaze-c-toolchain-pins"})
    with urllib.request.urlopen(request) as answer:
        return answer.read()


def commit():
    for line in open(RUST_PINS):
        if line.startswith("commit "):
            return line.split()[1]
    sys.exit(f"{os.path.relpath(RUST_PINS, ROOT)} has no commit line")


def pins():
    rust = commit()
    dockerfile = get(f"https://raw.githubusercontent.com/rust-lang/rust/{rust}/{DOCKERFILE}").decode()
    versions = sorted(set(re.findall(r"wasi-sdk-(\d+\.\d+)-x86_64-linux\.tar\.gz", dockerfile)))
    if len(versions) != 1:
        sys.exit(f"rust-lang/rust {rust[:12]}'s {DOCKERFILE} names wasi-sdk {versions or 'nowhere'}, "
                 "not one release")
    version = versions[0]
    tag = f"wasi-sdk-{version.split('.')[0]}"
    release = json.loads(get(f"https://api.github.com/repos/WebAssembly/wasi-sdk/releases/tags/{tag}"))
    assets = {it["name"]: it for it in release["assets"]}
    lines = [
        "# The C toolchain blaze compiles build scripts' C for wasm32 with (docs/c-toolchain.md):",
        "# written by harness/c_toolchain_pins.py '{\"write\": true}' from the wasi-sdk release",
        f"# rust-lang/rust {rust[:12]}'s {DOCKERFILE} builds wasi-libc with, and GitHub's",
        "# digests of its assets. wasi-sdk <version> <release address>; then per host",
        "# <host> wasi-sdk <file> <sha256> <bytes>.",
        f"wasi-sdk {version} {RELEASES}/{tag}",
    ]
    for host, theirs in HOSTS.items():
        name = f"wasi-sdk-{version}-{theirs}.tar.gz"
        asset = assets.get(name)
        if asset is None:
            sys.exit(f"wasi-sdk {tag} has no {name}")
        digest = asset.get("digest") or ""
        if not digest.startswith("sha256:"):
            sys.exit(f"GitHub publishes no sha256 for {tag}'s {name}")
        lines.append(f"{host} wasi-sdk {name} {digest.removeprefix('sha256:')} {asset['size']}")
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
        sys.exit(f"c_toolchain_pins.py: the input is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("c_toolchain_pins.py: the input is a JSON object")
    if unknown := sorted(set(value) - {f.name for f in fields(Pins)}):
        sys.exit(f"c_toolchain_pins.py: no field {', '.join(unknown)} (fields: write)")
    if not isinstance(value.get("write", False), bool):
        sys.exit(f"c_toolchain_pins.py: field write is a bool, not {value['write']!r}")
    return Pins(**value)


def main():
    if len(sys.argv) > 2:
        sys.exit(f"usage: c_toolchain_pins.py '<json>'\n\n{__doc__}")
    asked = typed(sys.argv[1] if len(sys.argv) == 2 else "")
    written = pins()
    if asked.write:
        with open(PINS, "w") as out:
            out.write(written)
        print(f"wrote {os.path.relpath(PINS, ROOT)}")
        return
    have = open(PINS).read() if os.path.exists(PINS) else ""
    if have != written:
        sys.exit(f"{os.path.relpath(PINS, ROOT)} is not the wasi-sdk the nightly builds with: "
                 "c_toolchain_pins.py '{\"write\": true}' writes it")
    print(f"{os.path.relpath(PINS, ROOT)} is the wasi-sdk the nightly builds with")


if __name__ == "__main__":
    main()
