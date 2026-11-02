#!/usr/bin/env python3
"""library.py: blaze's standard library source, library/ (docs/toolchain.md, "The standard library").

    library.py import '{"date": "2026-10-27"}'
        library/ becomes the standard library of that nightly as the Rust project publishes it,
        replacing what is there: its `rust-src` (library/ at the commit the nightly was built
        from, with the backtrace submodule, std's third-party crates in vendor/ and the
        .cargo/config.toml that reads them from there), checked against the sha256 the
        nightly's channel manifest gives; library/UPSTREAM says which. For the first import, or
        to start over; the daily bump merges instead. An empty date is harness/toolchain's.
    library.py merge '{"date": "2026-11-02"}'
        library/ moves to that nightly's standard library as a merge: upstream's changes from
        UPSTREAM's snapshot to the new nightly's are applied three-way to library/, so blaze's
        own changes are kept. A conflict stops with the files, which have conflict markers then,
        and nothing else changed is undone. An empty date is harness/toolchain's. The new
        snapshot is the branch upstream/library then, which the bump pushes.
    library.py snapshot '{}'
        prints the snapshot of UPSTREAM's nightly and makes the branch upstream/library hold
        it, so `git diff upstream/library -- library/` is blaze's changes to the standard library.
    library.py digest '{}'
        writes library/DIGEST, the digest blaze knows the library by (the store's key of the
        standard library; `toolchain/library.rs` computes the same): after any change to library/.
    library.py archive '{"out": "<dir>"}'
        writes <dir>/library-<digest>.tar.xz, the asset of a release a blaze without a checkout
        downloads the library from once, checking its files against the digest.

A snapshot is a commit of nothing but library/ as a nightly's rust-src has it (none of blaze's
own files), made the same way from the same rust-src and parent wherever it is made. Each one's
parent is the snapshot before it: the branch upstream/library on origin is upstream's standard
library, one commit a nightly, and the base of every merge.

The JSON is inline, a file's path, or `-` for stdin. Downloads are kept in ~/.cache/blaze-library.
Python 3.11+, stdlib, git and b3sum (BLAKE3's tool, for library/DIGEST).
"""
import hashlib, io, json, os, re, shutil, stat, subprocess, sys, tarfile, tempfile, tomllib, typing
import urllib.error, urllib.request
from dataclasses import dataclass, fields

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
LIBRARY = os.path.join(ROOT, "library")
UPSTREAM = os.path.join(LIBRARY, "UPSTREAM")
# blaze's files in library/, which no nightly has.
OURS = {"UPSTREAM", "DIGEST"}
DIGEST = os.path.join(LIBRARY, "DIGEST")
DIST = "https://static.rust-lang.org/dist"
# Where rust-src has the library in its tarball.
INSIDE = "rust-src-nightly/rust-src/lib/rustlib/src/rust/library/"
CACHE = os.path.join(os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache"), "blaze-library")


@dataclass(frozen=True)
class Nightly:
    date: str = ""


@dataclass(frozen=True)
class Empty:
    pass


@dataclass(frozen=True)
class Archive:
    out: str


def typed(cls, text):
    """`cls` from the JSON `text` (inline, a path, or `-`); exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif text and not text.lstrip().startswith("{"):
        text = open(text).read()
    try:
        value = json.loads(text or "{}")
    except json.JSONDecodeError as e:
        sys.exit(f"library.py: the input is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("library.py: the input is a JSON object")
    known = {f.name: f for f in fields(cls)}
    if unknown := sorted(set(value) - set(known)):
        sys.exit(f"library.py: no field {', '.join(unknown)} (fields: {', '.join(known) or 'none'})")
    hints = typing.get_type_hints(cls)
    for name, given in value.items():
        if not isinstance(given, hints[name]):
            sys.exit(f"library.py: field {name} is a {hints[name].__name__}, not {given!r}")
    if value.get("date") and not re.fullmatch(r"\d{4}-\d\d-\d\d", value["date"]):
        sys.exit(f"library.py: field date is YYYY-MM-DD, not {value['date']!r}")
    return cls(**value)


def pinned_date():
    name = open(os.path.join(HERE, "toolchain")).read().strip()
    return name.removeprefix("nightly-")


def get(url):
    try:
        with urllib.request.urlopen(url, timeout=120) as answer:
            return answer.read()
    except urllib.error.URLError as e:
        sys.exit(f"library.py: {url}: {e}")


@dataclass(frozen=True)
class Upstream:
    """A nightly's standard library: what UPSTREAM records."""
    nightly: str
    commit: str
    rustc: str
    url: str
    sha256: str

    def text(self):
        return (
            "# blaze's standard library (library/) is the Rust project's at this commit, as the\n"
            "# nightly's rust-src has it: library/ of rust-lang/rust with the backtrace submodule,\n"
            "# std's third-party crates in vendor/ and .cargo/config.toml. Written by\n"
            "# harness/library.py from the nightly's channel manifest.\n"
            "repo https://github.com/rust-lang/rust\n"
            f"commit {self.commit}\n"
            f"nightly {self.nightly}\n"
            f"rustc {self.rustc}\n"
            f"rust-src {self.url} {self.sha256}\n"
        )

    @staticmethod
    def read():
        if not os.path.isfile(UPSTREAM):
            sys.exit(f"library.py: {UPSTREAM} is not there: `library.py import` writes it")
        lines = dict(line.split(" ", 1) for line in open(UPSTREAM).read().splitlines() if line and not line.startswith("#"))
        url, sha256 = lines["rust-src"].split()
        return Upstream(lines["nightly"], lines["commit"], lines["rustc"], url, sha256)


# UPSTREAM from this line on is blaze's list of its changes to library/, kept through every rewrite.
NOTES = "# blaze's changes to library/"


def write_upstream(upstream):
    """UPSTREAM for `upstream`, with the notes of the file it replaces."""
    notes = ""
    if os.path.isfile(UPSTREAM):
        old = open(UPSTREAM).read()
        at = old.find("\n" + NOTES)
        if at >= 0:
            notes = "#\n" + old[at + 1:]
    open(UPSTREAM, "w").write(upstream.text() + notes)


def of_nightly(date):
    """The standard library of the nightly of `date`, from its channel manifest."""
    manifest = tomllib.loads(get(f"{DIST}/{date}/channel-rust-nightly.toml").decode())
    src = manifest["pkg"]["rust-src"]
    built = src["target"]["*"]
    if not built.get("available"):
        sys.exit(f"library.py: the nightly of {date} has no rust-src")
    version = src["version"]
    if not re.fullmatch(r"[0-9a-f]{40}", src.get("git_commit_hash") or ""):
        sys.exit(f"library.py: the nightly of {date}'s manifest names no commit for rust-src")
    return Upstream(f"nightly-{date}", src["git_commit_hash"], version, built["xz_url"], built["xz_hash"])


def tarball(upstream):
    """rust-src's .tar.xz of `upstream`, downloaded once and checked against its sha256."""
    os.makedirs(CACHE, exist_ok=True)
    kept = os.path.join(CACHE, f"rust-src-{upstream.sha256}.tar.xz")
    if os.path.isfile(kept):
        data = open(kept, "rb").read()
        if hashlib.sha256(data).hexdigest() == upstream.sha256:
            return data
    data = get(upstream.url)
    if (got := hashlib.sha256(data).hexdigest()) != upstream.sha256:
        sys.exit(f"library.py: {upstream.url} has sha256 {got}, and the manifest says {upstream.sha256}")
    open(kept + ".part", "wb").write(data)
    os.replace(kept + ".part", kept)
    return data


def unpack(upstream, into):
    """The library of `upstream`'s rust-src, written to `into`."""
    with tarfile.open(fileobj=io.BytesIO(tarball(upstream)), mode="r:xz") as tar:
        for member in tar.getmembers():
            if not member.name.startswith(INSIDE) or member.name == INSIDE:
                continue
            path = os.path.join(into, member.name[len(INSIDE):])
            if member.isdir():
                os.makedirs(path, exist_ok=True)
            elif member.issym():
                os.makedirs(os.path.dirname(path), exist_ok=True)
                os.symlink(member.linkname, path)
            elif member.isfile():
                os.makedirs(os.path.dirname(path), exist_ok=True)
                with open(path, "wb") as out:
                    shutil.copyfileobj(tar.extractfile(member), out)
                os.chmod(path, 0o755 if member.mode & 0o111 else 0o644)
            else:
                sys.exit(f"library.py: rust-src has {member.name}, which is neither a file, a link nor a directory")


def files(root):
    """{path relative to root: (kind, bytes)} of every file and link under `root`."""
    found = {}
    for dir, subdirs, names in os.walk(root):
        for name in names + [d for d in subdirs if os.path.islink(os.path.join(dir, d))]:
            full = os.path.join(dir, name)
            rel = os.path.relpath(full, root).replace(os.sep, "/")
            if os.path.islink(full):
                found[rel] = ("link", os.readlink(full).encode())
            else:
                mode = os.stat(full).st_mode
                found[rel] = ("exe" if mode & stat.S_IXUSR else "file", open(full, "rb").read())
    return found


def b3sum(args, data=None):
    """`b3sum`'s output for `args`, BLAKE3's own tool (SIMD, threads): blaze's hashes of its own
    data are BLAKE3, and Python's hashlib has none."""
    if shutil.which("b3sum") is None:
        sys.exit("library.py: no `b3sum` on PATH: library/DIGEST is a BLAKE3 hash, which b3sum "
                 "(the `b3sum` package of nixpkgs, Homebrew and Debian) computes")
    done = subprocess.run(["b3sum", "--no-names", *args], input=data, capture_output=True)
    if done.returncode:
        sys.exit(f"library.py: b3sum {' '.join(args[:3])}...: {done.stderr.decode()}")
    return done.stdout.decode().split()


def b3_of_files(paths):
    """The BLAKE3 hash, in hex, of each file of `paths`, in their order: 512 files a process."""
    hashes = []
    for at in range(0, len(paths), 512):
        batch = paths[at:at + 512]
        got = b3sum(["--", *batch])
        if len(got) != len(batch):
            sys.exit(f"library.py: b3sum gave {len(got)} hashes for {len(batch)} files")
        hashes += got
    return hashes


def digest_of(root):
    """The digest of the library at `root`: the BLAKE3 hash of a line per file and link under it
    but DIGEST, in the order of their paths' bytes, each `<kind> <BLAKE3 of its bytes, or of where
    a link points> <path>`, kind `file` or `link` (as crates/blaze-cargo's toolchain/library.rs
    computes it). Whether a file is executable is not in it: Windows does not say."""
    found = []
    for dir, subdirs, names in os.walk(root):
        for name in names + [d for d in subdirs if os.path.islink(os.path.join(dir, d))]:
            full = os.path.join(dir, name)
            path = os.path.relpath(full, root).replace(os.sep, "/")
            if path != "DIGEST":
                found.append((path.encode(), path, full))
    found.sort()
    plain = [full for _, _, full in found if not os.path.islink(full)]
    hashes = dict(zip(plain, b3_of_files(plain)))
    lines = []
    for _, path, full in found:
        if os.path.islink(full):
            lines.append(f"link {b3sum([], os.readlink(full).encode())[0]} {path}\n")
        else:
            lines.append(f"file {hashes[full]} {path}\n")
    return b3sum([], "".join(lines).encode())[0]


def write_digest():
    digest = digest_of(LIBRARY)
    open(DIGEST, "w").write(digest + "\n")
    return digest


def ours_kept():
    """blaze's files in library/, to put back over a fresh import."""
    return {name: open(os.path.join(LIBRARY, name), "rb").read()
            for name in OURS if os.path.isfile(os.path.join(LIBRARY, name))}


def do_import(args):
    upstream = of_nightly(args.date or pinned_date())
    kept = ours_kept()
    with tempfile.TemporaryDirectory(prefix="blaze-library-") as work:
        fresh = os.path.join(work, "library")
        unpack(upstream, fresh)
        shutil.rmtree(LIBRARY, ignore_errors=True)
        shutil.move(fresh, LIBRARY)
    for name, data in kept.items():
        open(os.path.join(LIBRARY, name), "wb").write(data)
    write_upstream(upstream)
    digest = write_digest()
    print(f"library/ is the standard library of {upstream.nightly} (rust-lang/rust {upstream.commit[:9]}), digest {digest[:12]}")


def git(*argv, check=True, input=None, env=None):
    env = {k: v for k, v in os.environ.items() if k != "GIT_EXTERNAL_DIFF"} | (env or {})
    done = subprocess.run(["git", *argv], cwd=ROOT, capture_output=True, input=input, env=env)
    if check and done.returncode != 0:
        sys.exit(f"library.py: `git {' '.join(argv)}` exited {done.returncode}: {done.stderr.decode()[-3000:]}")
    return done


SNAPSHOTS = "refs/heads/upstream/library"
ORIGIN_SNAPSHOTS = "refs/remotes/origin/upstream/library"


def out(*argv, **kw):
    return git(*argv, **kw).stdout.decode().strip()


def ref(name):
    return git("rev-parse", "-q", "--verify", f"{name}^{{commit}}", check=False).stdout.decode().strip() or None


def ahead_of(ours, theirs):
    return git("merge-base", "--is-ancestor", ours, theirs, check=False).returncode != 0


def snapshots_tip():
    """The newest snapshot: origin's upstream/library, or the local branch when it is ahead of it."""
    git("fetch", "-q", "origin", f"+{SNAPSHOTS}:{ORIGIN_SNAPSHOTS}", check=False)
    theirs, ours = ref(ORIGIN_SNAPSHOTS), ref(SNAPSHOTS)
    if theirs and ours and ahead_of(ours, theirs):
        return ours
    return theirs or ours


def snapshot(upstream):
    """The snapshot of `upstream`, made on the newest one when there is none yet; the local branch
    upstream/library holds the newest then."""
    tip = snapshots_tip()
    if tip:
        found = out("log", tip, "--format=%H", "-1", "--fixed-strings",
                    f"--grep=rust-src {upstream.url} {upstream.sha256}")
        if found:
            if not ref(SNAPSHOTS) or not ahead_of(ref(SNAPSHOTS), tip):
                git("update-ref", SNAPSHOTS, tip)
            return found
    with tempfile.TemporaryDirectory(prefix="blaze-library-") as work:
        root = os.path.join(work, "library")
        unpack(upstream, root)
        found = files(root)
        paths = sorted(found)
        plain = [p for p in paths if found[p][0] != "link"]
        sha = dict(zip(plain, out("hash-object", "-w", "--no-filters", "--stdin-paths",
                                  input="".join(os.path.join(root, p) + "\n" for p in plain).encode()).split()))
        for p in paths:
            if found[p][0] == "link":
                sha[p] = out("hash-object", "-w", "--no-filters", "--stdin", input=found[p][1])
        modes = {"file": "100644", "exe": "100755", "link": "120000"}
        index = {"GIT_INDEX_FILE": os.path.join(work, "index")}
        git("update-index", "--add", "--index-info", env=index,
            input="".join(f"{modes[found[p][0]]} {sha[p]}\tlibrary/{p}\n" for p in paths).encode())
        tree = out("write-tree", env=index)
    date = upstream.nightly.removeprefix("nightly-")
    who = {f"GIT_{role}_{what}": value for role in ("AUTHOR", "COMMITTER")
           for what, value in (("NAME", "harness/library.py"), ("EMAIL", "library@blaze.invalid"),
                               ("DATE", f"{date}T00:00:00Z"))}
    message = (f"library: {upstream.nightly}, rust-lang/rust {upstream.commit[:12]}\n\n"
               "The standard library as the nightly's rust-src has it, made by `harness/library.py`.\n\n"
               f"repo https://github.com/rust-lang/rust\ncommit {upstream.commit}\nnightly {upstream.nightly}\n"
               f"rustc {upstream.rustc}\nrust-src {upstream.url} {upstream.sha256}\n")
    made = out("commit-tree", tree, *(["-p", tip] if tip else []), "-m", message, env=who)
    git("update-ref", SNAPSHOTS, made)
    return made


def do_snapshot(args):
    upstream = Upstream.read()
    print(f"{snapshot(upstream)} upstream/library: {upstream.nightly}'s standard library "
          f"(rust-lang/rust {upstream.commit[:9]})")


def do_merge(args):
    old = Upstream.read()
    new = of_nightly(args.date or pinned_date())
    if new.commit == old.commit:
        print(f"library/ is already {new.nightly}'s ({new.commit[:9]})")
        write_upstream(new)
        snapshot(new)
        write_digest()
        return
    base = snapshot(old)
    theirs = snapshot(new)
    patch = git("diff", "--no-ext-diff", "--binary", "--no-color", "--no-renames", "--full-index",
                base, theirs, "--", "library").stdout
    if patch.strip():
        # Both sides' blobs are in this repository (the snapshots), so git merges each file
        # three-way and leaves conflict markers where blaze and upstream changed the same lines.
        applied = git("apply", "--3way", "--whitespace=nowarn", "-", check=False, input=patch)
        if applied.returncode != 0:
            conflicted = git("diff", "--name-only", "--diff-filter=U", check=False).stdout.decode().split()
            sys.exit(
                f"library.py: upstream's changes from {old.nightly} to {new.nightly} conflict with blaze's "
                f"changes to the standard library in: {', '.join(conflicted) or '(see below)'}. "
                f"Resolve the conflict markers there, `git add` them and finish the bump; upstream's whole "
                f"change is `git diff {base[:12]} {theirs[:12]}`. git apply said:\n{applied.stderr.decode()[-3000:]}")
    write_upstream(new)
    write_digest()
    print(f"library/ merged {old.nightly} ({old.commit[:9]}) -> {new.nightly} ({new.commit[:9]}); "
          f"upstream/library is {theirs[:12]}: `git push origin upstream/library`")


def do_digest(args):
    print(f"library/DIGEST: {write_digest()}")


def do_archive(args):
    digest = open(DIGEST).read().strip()
    if digest_of(LIBRARY) != digest:
        sys.exit("library.py: library/DIGEST is not library/'s digest: `library.py digest '{}'` writes it")
    os.makedirs(args.out, exist_ok=True)
    out = os.path.join(args.out, f"library-{digest}.tar.xz")

    def tidy(info):
        info.uid = info.gid = 0
        info.uname = info.gname = ""
        info.mtime = 0
        if info.isfile():
            info.mode = 0o755 if info.mode & 0o100 else 0o644
        elif info.isdir():
            info.mode = 0o755
        return info

    with tarfile.open(out + ".part", "w:xz", preset=9) as tar:
        for dir, subdirs, names in os.walk(LIBRARY):
            subdirs.sort()
            rel = os.path.relpath(dir, ROOT).replace(os.sep, "/")
            tar.add(dir, arcname=rel, recursive=False, filter=tidy)
            for name in sorted(names + [d for d in subdirs if os.path.islink(os.path.join(dir, d))]):
                tar.add(os.path.join(dir, name), arcname=f"{rel}/{name}", recursive=False, filter=tidy)
    os.replace(out + ".part", out)
    print(out)


def main():
    commands = {"import": (Nightly, do_import), "merge": (Nightly, do_merge), "snapshot": (Empty, do_snapshot),
                "digest": (Empty, do_digest),
                "archive": (Archive, do_archive)}
    if len(sys.argv) not in (2, 3) or sys.argv[1] not in commands:
        sys.exit(f"usage: library.py {{{'|'.join(commands)}}} '<json>'\n\n{__doc__}")
    cls, run = commands[sys.argv[1]]
    run(typed(cls, sys.argv[2] if len(sys.argv) == 3 else ""))


if __name__ == "__main__":
    main()
