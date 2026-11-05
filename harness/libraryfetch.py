#!/usr/bin/env python3
"""libraryfetch.py: blaze installs its standard library from the release's archive, checked.

A blaze with no checkout of its commit downloads its library/ once as the release's asset
`library-<digest>.tar.xz` (docs/toolchain.md, "The standard library: library/"). The archive is
made here as the release workflow makes it (`library.py archive`), served from this process at
a release's path, and `blaze toolchain` is run with a home of its own and `BLAZE_LIBRARY_FROM`
naming the server, which it takes over the checkout:

  fetched   it downloads the archive once, says so, and its copy has the digest blaze knows
  kept      again in the same home: nothing is asked of the server and nothing is said
  lying     an archive whose files are not what the digest says is turned down, saying so,
            and nothing is kept
  missing   a release without the archive is an error naming where it asked

usage: libraryfetch.py    (BLAZE_BIN names the blaze built; blaze is found next to it)

Python 3, stdlib only.
"""
import http.server, io, os, subprocess, sys, tarfile, tempfile, threading

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from blaze_cli import BLAZE, EXE, base_env
from library import DIGEST, LIBRARY, digest_of

TIMEOUT = 600
RELEASE = "releases/download/v0.0.0-libraryfetch"


def serve(root):
    """The address of an HTTP server of the files in `root`, and the paths it was asked for."""
    asked = []

    class Files(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, directory=root, **kwargs)

        def send_head(self):
            asked.append(self.path)
            return super().send_head()

        def log_message(self, *_):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Files)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return f"http://127.0.0.1:{server.server_address[1]}", asked


def lying_archive(path, digest):
    """An archive of a library whose DIGEST says `digest` and whose files are not that library."""
    files = {"Cargo.toml": b"[workspace]\n", "core/src/lib.rs": b"// not core\n",
             "UPSTREAM": open(os.path.join(LIBRARY, "UPSTREAM"), "rb").read(),
             "DIGEST": f"{digest}\n".encode()}
    with tarfile.open(path, "w:xz") as tar:
        for name, data in files.items():
            info = tarfile.TarInfo(f"library/{name}")
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))


def toolchain(blaze, home, source):
    env = base_env(home)
    env.pop("BLAZE_LIBRARY", None)
    env["BLAZE_LIBRARY_FROM"] = source
    return subprocess.run([blaze, "toolchain"], capture_output=True, text=True, env=env, timeout=TIMEOUT)


def main():
    blaze = os.path.join(os.path.dirname(BLAZE), "blaze" + EXE)
    if not os.path.isfile(blaze):
        sys.exit(f"no blaze beside {BLAZE}")
    digest = open(DIGEST).read().strip()
    archive = f"library-{digest}.tar.xz"
    failed = []

    def check(what, ok, run):
        print(f"{'ok  ' if ok else 'FAIL'} {what}")
        if not ok:
            failed.append(what)
            print(f"  exit {run.returncode}; stderr:\n{run.stderr[-3000:]}")

    with tempfile.TemporaryDirectory(prefix="blaze-libraryfetch-") as work:
        good, lying = os.path.join(work, "served", RELEASE), os.path.join(work, "served", "lying")
        os.makedirs(lying)
        made = subprocess.run([sys.executable, "-I", os.path.join(HERE, "library.py"), "archive",
                               f'{{"out": "{good}"}}'], capture_output=True, text=True)
        if made.returncode != 0 or not os.path.isfile(os.path.join(good, archive)):
            sys.exit(f"library.py archive did not write {archive}:\n{made.stdout}{made.stderr}")
        lying_archive(os.path.join(lying, archive), digest)
        address, asked = serve(os.path.join(work, "served"))
        release = f"{address}/{RELEASE}"
        wanted = f"/{RELEASE}/{archive}"

        home = os.path.join(work, "home")
        kept = os.path.join(home, "library", digest, "library")
        run = toolchain(blaze, home, release)
        check("fetched: the archive is downloaded once, said, and kept with blaze's digest",
              run.returncode == 0 and asked == [wanted]
              and f"Downloading blaze's standard library (digest {digest[:12]}, once) from {release}" in run.stderr
              and os.path.isdir(kept) and digest_of(kept) == digest, run)

        del asked[:]
        run = toolchain(blaze, home, release)
        check("kept: nothing is asked again nor said", run.returncode == 0 and not asked
              and "Downloading" not in run.stderr, run)

        home = os.path.join(work, "home-lying")
        run = toolchain(blaze, home, f"{address}/lying")
        check("lying: an archive that is not blaze's library is turned down and not kept",
              run.returncode != 0 and "is not the library blaze was built with" in run.stderr
              and not os.path.exists(os.path.join(home, "library", digest, "library")), run)

        run = toolchain(blaze, os.path.join(work, "home-missing"), f"{address}/missing")
        check("missing: a release without the archive is an error naming where it asked",
              run.returncode != 0 and f"getting it from {address}/missing failed" in run.stderr, run)

    if failed:
        sys.exit(f"libraryfetch.py: {len(failed)} failed: {'; '.join(failed)}")


if __name__ == "__main__":
    main()
