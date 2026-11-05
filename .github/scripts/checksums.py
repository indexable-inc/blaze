"""checksums.py: SHA256SUMS and B3SUMS for a release's archives, the lines sha256sum and b3sum write.

    uv run --frozen python .github/scripts/checksums.py '{"dir": "out"}'

Each file is `<hex>  <name>` a line, one per `blaze-*.tar.xz` and `blaze-*.tar.gz` in `dir`, in name order. BLAKE3 is
the uv workspace's `blake3` (the BLAKE3 team's Rust crate through PyO3), the hash blaze checks a
download against (crates/blaze-engine/src/versions.rs), so a runner needs no system package.
Exits 1 naming the directory when it holds no archive.
"""

import hashlib
import sys
from dataclasses import dataclass
from pathlib import Path

from blake3 import blake3
from typed_input import parse


@dataclass(frozen=True)
class Checksums:
    # The directory holding the archives; the two lists are written beside them.
    dir: str


def main() -> int:
    job = parse(Checksums, sys.argv[1] if len(sys.argv) > 1 else "{}", "checksums.py")
    root = Path(job.dir)
    archives = sorted([*root.glob("blaze-*.tar.xz"), *root.glob("blaze-*.tar.gz")])
    if not archives:
        print(f"checksums.py: no blaze-*.tar.xz or blaze-*.tar.gz in {root.resolve()}", file=sys.stderr)
        return 1
    sha, b3 = [], []
    for archive in archives:
        data = archive.read_bytes()
        sha.append(f"{hashlib.sha256(data).hexdigest()}  {archive.name}\n")
        b3.append(f"{blake3(data, max_threads=blake3.AUTO).hexdigest()}  {archive.name}\n")
    for name, lines in (("SHA256SUMS", sha), ("B3SUMS", b3)):
        (root / name).write_text("".join(lines))
        print(f"{name}:\n{''.join(lines)}", end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
