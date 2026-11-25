#!/usr/bin/env python3
"""Installer regression: a v2 release's checked toolchain shelf is installed."""

import hashlib
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile


def main() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        release = root / "release/v9.9.9"
        release.mkdir(parents=True)
        target = "x86_64-unknown-linux-gnu"
        package = f"blaze-v9.9.9-{target}"
        binary = root / package / "blaze"
        binary.parent.mkdir()
        binary.write_text('#!/bin/sh\n[ "$1" = toolchain ] && echo "Rust test-toolchain"\n')
        binary.chmod(0o755)
        with tarfile.open(release / f"{package}.tar.gz", "w:gz") as tar:
            tar.add(binary.parent, arcname=package)

        record_name = "a" * 64
        record = root / "record"
        record.write_bytes(b"test toolchain record\n")
        with tarfile.open(release / "toolchain-v9.9.9.tar.xz", "w:xz") as tar:
            tar.add(record, arcname=f"./{record_name}")
        archives = [release / f"{package}.tar.gz", release / "toolchain-v9.9.9.tar.xz"]
        (release / "SHA256SUMS").write_text("".join(
            f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n" for path in archives
        ))

        curl = root / "bin/curl"
        curl.parent.mkdir()
        mock = "#!/usr/bin/env python3\nimport pathlib, shutil, sys\n"
        mock += "out = sys.argv[sys.argv.index('-o') + 1]\nname = sys.argv[-1].rsplit('/', 1)[-1]\n"
        mock += f"source = pathlib.Path({str(release)!r}) / name\n"
        mock += "sys.exit(22) if not source.exists() else shutil.copyfile(source, out)\n"
        curl.write_text(mock)
        curl.chmod(0o755)
        home = root / "home"
        home.mkdir()
        env = os.environ.copy()
        env.update({"HOME": str(home), "CARGO_HOME": str(home / "cargo"), "PATH": f"{curl.parent}:{env['PATH']}"})
        for _ in range(2):
            result = subprocess.run(
                ["sh", str(Path(__file__).with_name("install.sh")), "--version", "9.9.9"],
                env=env, capture_output=True, text=True, timeout=30,
            )
            assert result.returncode == 0, result.stdout + result.stderr
            installed = home / ".blaze/v2/toolchain" / record_name
            assert installed.read_bytes() == record.read_bytes()
            assert (home / "cargo/bin/blaze").is_file()
        print("toolchain shelf install and idempotent reinstall: PASS")


if __name__ == "__main__":
    main()
