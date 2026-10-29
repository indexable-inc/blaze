#!/usr/bin/env python3
"""fetchdiff.py: blaze-registry's fetching against cargo's, package by package.

usage: fetchdiff.py [--work DIR] [--popular N] [--projects NAME,...] [--vendor NAME,...]
                    [--blaze FETCH]

The lock files are the bench projects' (bench.py's PROJECTS, each at its pinned revision: its
Cargo.lock, or the one cargo makes when it has none; not bun's, whose tree bunprep.py makes),
one for each of the N most downloaded crates of crates.io (default 300: a project depending on
its newest version, locked by cargo), one of git dependencies of every kind of reference, and
one of a few crates from each of two more sparse registries,
which this script serves on 127.0.0.1 from crates.io's index and files under a `dl` with no
markers. The second of them requires a token (`auth-required`, a 401 with a `Cargo`
challenge to whatever does not send it, `config.json` too), which both homes have in
credentials.toml with `cargo:token` in `registry.global-credential-providers`: every request
to it but the first `config.json` has to carry the token, for cargo and blaze alike. The pinned nightly's cargo fetches every lock file (`cargo fetch --locked`) into a CARGO_HOME of its
own, and blaze-registry's `fetch` example (FETCH, or built here) into another. Every package
either unpacked into registry/src is compared: equal when it has the same directory in both and
the same tree (every path, its kind and mode, a file's size, sha256 and mtime, a link's target;
the mtime of `.cargo-ok` and of directories is when they were made), and its .crate file in
registry/cache the same bytes. The names of the directories in registry/index, registry/cache
and registry/src have to be the same too.

Then the same of a project whose .cargo/config.toml replaces crates.io with that registry
(`[source]`), in two more homes; and of the projects of --vendor (default ripgrep and zed)
vendored by `cargo vendor`, which blaze has to read where cargo vendored them, every file
checked against `.cargo-checksum.json`, downloading nothing.

Git dependencies are compared by checkout: the names of the directories in git/db and
git/checkouts, and the tree of every checkout but `.git` and mtimes.

Prints the counts and writes every difference to WORK/fetchdiff.json. Network-heavy: run it in
an ix VM.
"""
import argparse, ast, hashlib, http.server, json, os, shutil, stat, subprocess, sys, threading, time
import urllib.error, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
PIN = open(os.path.join(HERE, "toolchain")).read().strip()
TOOLCHAIN = PIN
AGENT = "blaze harness/fetchdiff.py (https://github.com/indexable-inc/blaze)"
MIRROR_PORT = 48273
MIRROR = f"sparse+http://127.0.0.1:{MIRROR_PORT}/index/"
MIRROR_CRATES = {"serde": "1", "regex": "1", "tokio": "1", "clap": "4", "rand": "0.9"}
AUTH_MIRROR = f"sparse+http://127.0.0.1:{MIRROR_PORT}/auth/index/"
AUTH_TOKEN = "fetchdiff-token"
AUTH_CRATES = {"itoa": "1", "ryu": "1", "memchr": "2"}
AUTH_CONFIG = (f'[registries.mirror]\nindex = "{MIRROR}"\n\n[registries.authmirror]\nindex = "{AUTH_MIRROR}"\n\n'
               '[registry]\nglobal-credential-providers = ["cargo:token"]\n')
AUTH_CREDENTIALS = f'[registries.authmirror]\ntoken = "{AUTH_TOKEN}"\n'
# Requests to the registry that requires a token: (client, path, whether it had the token).
AUTH_ASKED = []
GIT_DEPS = [
    'curl-sys = { git = "https://github.com/alexcrichton/curl-rust", branch = "main" }',
    'anyhow = { git = "https://github.com/dtolnay/anyhow", tag = "1.0.80" }',
    'ryu = { git = "https://github.com/dtolnay/ryu", rev = "1.0.17" }',
    'itoa = { git = "https://github.com/dtolnay/itoa" }',
]


def projects():
    """bench.py's PROJECTS as name -> (repo, rev, prepare), read from its source: importing
    bench.py asks for the toolchain blaze builds with."""
    tree = ast.parse(open(os.path.join(HERE, "bench.py")).read())
    for node in tree.body:
        if isinstance(node, ast.Assign) and any(getattr(it, "id", None) == "PROJECTS" for it in node.targets):
            found = {}
            for key, value in zip(node.value.keys, node.value.values):
                fields = {kw.arg: kw.value.value for kw in value.keywords if isinstance(kw.value, ast.Constant)}
                found[key.value] = (fields["repo"], fields["rev"], fields.get("prepare"))
            return found
    sys.exit("fetchdiff: no PROJECTS in bench.py")


def tool(name):
    return subprocess.run(["rustup", "which", name, "--toolchain", TOOLCHAIN], capture_output=True, text=True,
                          check=True).stdout.strip()


def get(url, headers=()):
    request = urllib.request.Request(url, headers={"user-agent": AGENT, **dict(headers)})
    with urllib.request.urlopen(request, timeout=60) as response:
        return response.read()


class Mirror(http.server.BaseHTTPRequestHandler):
    """A sparse registry of its own URL with crates.io's packages: its index files are
    crates.io's (whose dependencies name no registry, so this one), its `dl` has no markers."""

    def do_GET(self):
        path = self.path
        if path.startswith("/auth/"):
            sent = self.headers.get("authorization") == AUTH_TOKEN
            client = "blaze" if self.headers.get("user-agent", "").startswith("blaze") else "cargo"
            AUTH_ASKED.append((client, path, sent))
            if not sent:
                body = b"this registry wants a token"
                self.send_response(401)
                self.send_header("www-authenticate", f'Cargo login_url="http://127.0.0.1:{MIRROR_PORT}/me"')
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                return self.wfile.write(body)
            path = path[len("/auth"):]
        try:
            if path == "/index/config.json":
                config = {"dl": f"http://127.0.0.1:{MIRROR_PORT}{self.path[:len(self.path) - len(path)]}/dl"}
                if self.path != path:
                    config["auth-required"] = True
                body = json.dumps(config).encode()
            elif path.startswith("/index/"):
                body = get("https://index.crates.io/" + path[len("/index/"):])
            elif path.startswith("/dl/") and path.endswith("/download"):
                name, version = path.split("/")[2:4]
                body = get(f"https://static.crates.io/crates/{name}/{name}-{version}.crate")
            else:
                return self.send_error(404)
        except urllib.error.HTTPError as err:
            return self.send_error(err.code)
        except OSError:
            return self.send_error(502)
        etag = '"' + hashlib.sha256(body).hexdigest() + '"'
        try:
            if self.headers.get("if-none-match") == etag:
                self.send_response(304)
                self.end_headers()
                return
            self.send_response(200)
            self.send_header("content-length", str(len(body)))
            self.send_header("etag", etag)
            self.end_headers()
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, *args):
        pass


class Cargo:
    def __init__(self, home):
        self.home = home
        self.env = {**os.environ, "CARGO_HOME": home, "RUSTC": tool("rustc"), "CARGO_TERM_COLOR": "never"}
        self.cargo = tool("cargo")

    def run(self, args, cwd):
        return subprocess.run([self.cargo, *args], cwd=cwd, env=self.env, capture_output=True, text=True)


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)


def project(dir, deps, cargo):
    """A package depending on `deps` (TOML lines), locked by cargo. Returns cargo's error, if any."""
    write(os.path.join(dir, "Cargo.toml"),
          '[package]\nname = "fetchdiff"\nversion = "0.0.0"\nedition = "2021"\n\n[dependencies]\n' + deps)
    write(os.path.join(dir, "src", "lib.rs"), "")
    if not os.path.exists(os.path.join(dir, "Cargo.lock")):
        made = cargo.run(["generate-lockfile"], dir)
        if made.returncode:
            return made.stderr.strip()[-2000:]
    return None


def popular(count):
    """The `count` most downloaded crates of crates.io, with their newest stable versions."""
    found = []
    page = 1
    while len(found) < count:
        listed = json.loads(get(f"https://crates.io/api/v1/crates?sort=downloads&per_page=100&page={page}"))
        if not listed["crates"]:
            break
        found += [(it["name"], it["max_stable_version"]) for it in listed["crates"] if it.get("max_stable_version")]
        page += 1
        time.sleep(1)
    return found[:count]


def clone(dir, repo, rev):
    if not os.path.exists(os.path.join(dir, ".git")):
        os.makedirs(dir, exist_ok=True)
        for args in (["init", "-q"], ["fetch", "-q", "--depth", "1", f"https://github.com/{repo}", rev],
                     ["checkout", "-q", "FETCH_HEAD"]):
            subprocess.run(["git", *args], cwd=dir, check=True)


def packages(lock):
    """A lock file's packages as (source, name, version), with the git ones apart."""
    import tomllib
    with open(lock, "rb") as f:
        listed = tomllib.load(f).get("package", [])
    registry, git = set(), set()
    for it in listed:
        source = it.get("source")
        if source and source.startswith(("registry+", "sparse+")):
            registry.add((source, it["name"], it["version"]))
        elif source and source.startswith("git+"):
            git.add((source, it["name"], it["version"]))
    return registry, git


def tree(root, checkout=False):
    """Every path under `root` with what has to be equal of it; of a git checkout, not `.git`
    and not mtimes, which are when it was checked out."""
    found = {}
    for dirpath, dirnames, filenames in os.walk(root):
        if checkout:
            dirnames[:] = [it for it in dirnames if it != ".git"]
            filenames = [it for it in filenames if it != ".git"]
        dirnames.sort()
        for name in dirnames + filenames:
            path = os.path.join(dirpath, name)
            rel = os.path.relpath(path, root)
            st = os.lstat(path)
            mode = stat.S_IMODE(st.st_mode)
            if stat.S_ISLNK(st.st_mode):
                found[rel] = ("link", os.readlink(path))
            elif stat.S_ISDIR(st.st_mode):
                found[rel] = ("dir", oct(mode))
            elif name == ".cargo-ok" and (dirpath == root or checkout):
                found[rel] = ("file", oct(mode), open(path, "rb").read().decode(errors="replace"))
            else:
                found[rel] = ("file", oct(mode), st.st_size, sha256(path), None if checkout else int(st.st_mtime))
    return found


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            digest.update(chunk)
    return digest.hexdigest()


def listing(home, *parts, under="registry"):
    dir = os.path.join(home, under, *parts)
    return sorted(os.listdir(dir)) if os.path.isdir(dir) else []


def compare_git(cargo_home, blaze_home):
    """Every checkout under git/checkouts of either home, and the names of git/db's and
    git/checkouts' directories."""
    unequal, total = {}, 0
    for ident in sorted(set(listing(cargo_home, "checkouts", under="git")) | set(listing(blaze_home, "checkouts", under="git"))):
        both = [os.path.join(home, "git", "checkouts", ident) for home in (cargo_home, blaze_home)]
        for short in sorted(set(listing(cargo_home, "checkouts", ident, under="git")) | set(listing(blaze_home, "checkouts", ident, under="git"))):
            total += 1
            left, right = (os.path.join(it, short) for it in both)
            if not os.path.isdir(left) or not os.path.isdir(right):
                unequal[f"{ident}/{short}"] = [f"only {'blaze' if os.path.isdir(right) else 'cargo'} has it"]
                continue
            a, b = tree(left, checkout=True), tree(right, checkout=True)
            if found := [f"{rel}: cargo {a.get(rel)}, blaze {b.get(rel)}" for rel in sorted(a.keys() | b.keys()) if a.get(rel) != b.get(rel)]:
                unequal[f"{ident}/{short}"] = found
    dirs = {part: (listing(cargo_home, part, under="git"), listing(blaze_home, part, under="git")) for part in ("db", "checkouts")}
    return total, unequal, dirs


def differences(cargo_home, blaze_home, dir, package):
    left = os.path.join(cargo_home, "registry", "src", dir, package)
    right = os.path.join(blaze_home, "registry", "src", dir, package)
    if not os.path.isdir(right):
        return [f"blaze has no {dir}/{package}"]
    if not os.path.isdir(left):
        return [f"cargo has no {dir}/{package}"]
    found = []
    a, b = tree(left), tree(right)
    for rel in sorted(a.keys() | b.keys()):
        if a.get(rel) != b.get(rel):
            found.append(f"{rel}: cargo {a.get(rel)}, blaze {b.get(rel)}")
    crate = os.path.join("registry", "cache", dir, f"{package}.crate")
    have = [sha256(os.path.join(home, crate)) if os.path.exists(os.path.join(home, crate)) else None
            for home in (cargo_home, blaze_home)]
    if have[0] != have[1]:
        found.append(f"{crate}: cargo {have[0]}, blaze {have[1]}")
    return found


def compare(cargo_home, blaze_home):
    """Every package unpacked under registry/src of either home, and the names of the
    directories registry/index, registry/cache and registry/src have."""
    unequal, total = {}, 0
    for dir in sorted(set(listing(cargo_home, "src")) | set(listing(blaze_home, "src"))):
        for package in sorted(set(listing(cargo_home, "src", dir)) | set(listing(blaze_home, "src", dir))):
            total += 1
            if found := differences(cargo_home, blaze_home, dir, package):
                unequal[f"{dir}/{package}"] = found
    dirs = {part: (listing(cargo_home, part), listing(blaze_home, part)) for part in ("index", "cache", "src")}
    return total, unequal, dirs


def auth_asked(client):
    """What `client` asked of the registry that requires a token, and whether that was as it
    should be: `config.json` first with no token and then with it, everything else with it."""
    asked = [(path, sent) for who, path, sent in AUTH_ASKED if who == client]
    config = [sent for path, sent in asked if path.endswith("/config.json")]
    good = (bool(asked) and all(sent for path, sent in asked if not path.endswith("/config.json"))
            and all(config[1:]) and (config[:1] != [False] or len(config) > 1))
    return {"asked": len(asked), "with_token": sum(sent for _, sent in asked),
            "crates": sum(path.endswith("/download") for path, _ in asked), "as_cargo_sends": good}


def fresh(*homes):
    for home in homes:
        shutil.rmtree(home, ignore_errors=True)
        os.makedirs(home)


def blaze_fetch(given):
    if given:
        return given
    target = os.environ.get("CARGO_TARGET_DIR") or os.path.join(ROOT, "target")
    subprocess.run(["cargo", f"+{TOOLCHAIN}", "build", "--release", "-q", "-p", "blaze-registry", "--example", "fetch"],
                   cwd=ROOT, check=True)
    return os.path.join(target, "release", "examples", "fetch")


class Run:
    def __init__(self, fetch):
        self.fetch = fetch
        self.failed = {}
        self.seconds = {"cargo": 0.0, "blaze": 0.0}
        self.blaze = {}

    def both(self, name, dir, cargo, blaze_home, config=()):
        """`cargo fetch --locked` of the project at `dir`, then blaze's of its lock file. Returns
        whether both did."""
        start = time.monotonic()
        ran = cargo.run(["fetch", "--locked"], dir)
        self.seconds["cargo"] += time.monotonic() - start
        if ran.returncode:
            self.failed[name] = "cargo fetch: " + ran.stderr.strip()[-2000:]
            return False
        return self.blaze_fetch(name, dir, blaze_home, config) is not None

    def blaze_fetch(self, name, dir, blaze_home, config=(), roots=False):
        start = time.monotonic()
        command = [self.fetch, os.path.join(dir, "Cargo.lock"), blaze_home]
        home_config = os.path.join(blaze_home, "config.toml")
        if os.path.isfile(home_config):
            command += ["--config", home_config]
        for it in config:
            command += ["--config", it]
        ran = subprocess.run(command + (["--roots"] if roots else []), capture_output=True, text=True)
        self.seconds["blaze"] += time.monotonic() - start
        if ran.returncode:
            self.failed[name] = "blaze fetch: " + ran.stderr.strip()[-2000:]
            return None
        lines = ran.stdout.strip().splitlines()
        for kind, count in json.loads(lines[-1]).items():
            self.blaze[kind] = self.blaze.get(kind, 0) + count
        return lines[:-1]


def vendored(run, name, dir, cargo, work):
    """`cargo vendor` of the project at `dir`, and blaze's fetch of its lock file from what that
    vendored: (packages, how many blaze read where cargo vendored them, once their files were
    checked)."""
    out = os.path.join(work, "vendored", name)
    shutil.rmtree(out, ignore_errors=True)
    made = cargo.run(["vendor", "--locked", "--versioned-dirs", os.path.join(out, "vendor")], dir)
    if made.returncode:
        run.failed[f"vendor {name}"] = "cargo vendor: " + made.stderr.strip()[-2000:]
        return 0, 0
    config = os.path.join(out, ".cargo", "config.toml")
    write(config, made.stdout)
    home = os.path.join(work, "blaze-home-vendored")
    fresh(home)
    roots = run.blaze_fetch(f"vendor {name}", dir, home, [config], roots=True)
    if roots is None:
        return 0, 0
    total = equal = 0
    for line in roots:
        package, root = line.rsplit(" ", 1)
        name_, version = package.split(" v", 1)
        total += 1
        equal += root == os.path.join(out, "vendor", f"{name_}-{version}")
    if listing(home, "src"):
        run.failed[f"vendor {name}"] = "blaze unpacked packages it had vendored"
    return total, equal


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--work", default=os.path.abspath("fetchdiff-work"))
    parser.add_argument("--popular", type=int, default=300)
    parser.add_argument("--projects", help="names of bench.py's PROJECTS, all of them without")
    parser.add_argument("--vendor", default="ripgrep,zed", help="the projects vendored too")
    parser.add_argument("--git-index-only", action="store_true", help="compare the local git-index fixture, with no external downloads")
    parser.add_argument("--git-index", action="store_true", help="read crates.io through its git index for the full corpus")
    parser.add_argument("--blaze", help="blaze-registry's `fetch` example, built here without")
    args = parser.parse_args()
    if args.git_index_only:
        from gitindex import main as git_index
        git_index(blaze_fetch(args.blaze))
        return
    work = os.path.abspath(args.work)
    run = Run(blaze_fetch(args.blaze))
    cargo_home, blaze_home = os.path.join(work, "cargo-home"), os.path.join(work, "blaze-home")
    fresh(cargo_home, blaze_home)
    for home in cargo_home, blaze_home:
        config = AUTH_CONFIG
        if args.git_index:
            config += '\n[registries.crates-io]\nprotocol = "git"\n[net]\ngit-fetch-with-cli = true\n'
        write(os.path.join(home, "config.toml"), config)
        write(os.path.join(home, "credentials.toml"), AUTH_CREDENTIALS)
    cargo = Cargo(cargo_home)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", MIRROR_PORT), Mirror)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    locks = {}
    names = args.projects.split(",") if args.projects is not None else None
    for name, (repo, rev, prepare) in projects().items():
        if names is not None and name not in names:
            continue
        if prepare:
            run.failed[name] = f"fetches from the tree {prepare}.py makes"
            continue
        dir = os.path.join(work, "projects", name)
        clone(dir, repo, rev)
        if not os.path.exists(os.path.join(dir, "Cargo.toml")):
            run.failed[name] = "no Cargo.toml at the root"
            continue
        if not os.path.exists(os.path.join(dir, "Cargo.lock")):
            made = cargo.run(["generate-lockfile"], dir)
            if made.returncode:
                run.failed[name] = made.stderr.strip()[-2000:]
                continue
        locks[name] = dir
    for name, version in popular(args.popular) if args.popular else ():
        dir = os.path.join(work, "popular", name)
        if error := project(dir, f'{name} = "={version}"\n', cargo):
            run.failed[f"{name} {version}"] = error
        else:
            locks[f"{name} {version}"] = dir
    dir = os.path.join(work, "mirror")
    deps = "".join(f'{name} = {{ version = "{req}", registry = "mirror" }}\n' for name, req in MIRROR_CRATES.items())
    if error := project(dir, deps, cargo):
        run.failed["mirror"] = error
    else:
        locks["mirror"] = dir
    dir = os.path.join(work, "authmirror")
    deps = "".join(f'{name} = {{ version = "{req}", registry = "authmirror" }}\n' for name, req in AUTH_CRATES.items())
    if error := project(dir, deps, cargo):
        run.failed["authmirror"] = error
    else:
        locks["authmirror"] = dir

    # Git dependencies of every kind of reference: a branch (with a submodule), a tag, a tag
    # named as `rev`, the default branch, and a repository on no forge (a file URL), which
    # GitHub's fast path does not answer for.
    repo = os.path.join(work, "gitrepos", "semver.git")
    if not os.path.exists(repo):
        subprocess.run(["git", "clone", "-q", "--bare", "https://github.com/dtolnay/semver", repo], check=True)
    dir = os.path.join(work, "gitdeps")
    deps = "".join(f"{line}\n" for line in GIT_DEPS) + f'semver-file = {{ package = "semver", git = "file://{repo}" }}\n'
    if error := project(dir, deps, cargo):
        run.failed["gitdeps"] = error
    else:
        locks["gitdeps"] = dir

    registry, git = set(), set()
    auth = {}
    for name, dir in locks.items():
        if name == "authmirror":
            AUTH_ASKED.clear()
        if run.both(name, dir, cargo, blaze_home):
            found, from_git = packages(os.path.join(dir, "Cargo.lock"))
            registry |= found
            git |= from_git
        if name == "authmirror":
            auth = {client: auth_asked(client) for client in ("blaze")}
    total, unequal, dirs = compare(cargo_home, blaze_home)
    git_total, git_unequal, git_dirs = compare_git(cargo_home, blaze_home)
    dirs.update({f"git/{part}": it for part, it in git_dirs.items()})

    # crates.io replaced by the mirror, in the project's own .cargo/config.toml: both unpack
    # under the mirror's directory.
    replaced = {"total": 0, "unequal": {}, "dirs": {}}
    dir = os.path.join(work, "replaced")
    config = os.path.join(dir, ".cargo", "config.toml")
    write(config, f'[source.crates-io]\nreplace-with = "m"\n\n[source.m]\nregistry = "{MIRROR}"\n')
    homes = os.path.join(work, "cargo-home-replaced"), os.path.join(work, "blaze-home-replaced")
    fresh(*homes)
    other = Cargo(homes[0])
    deps = "".join(f'{name} = "{req}"\n' for name, req in MIRROR_CRATES.items())
    if error := project(dir, deps, other):
        run.failed["replaced"] = error
    elif run.both("replaced", dir, other, homes[1], [config]):
        replaced["total"], replaced["unequal"], replaced["dirs"] = compare(*homes)

    vendor = {}
    for name in args.vendor.split(",") if args.vendor else ():
        if name in locks:
            vendor[name] = vendored(run, name, locks[name], cargo, work)
    server.shutdown()

    dirs_equal = all(a == b for a, b in [*dirs.values(), *replaced["dirs"].values()])
    result = {
        "locks": len(locks), "lock_packages": len(registry), "git": len(git),
        "packages": total, "equal": total - len(unequal), "dirs": dirs,
        "checkouts": git_total, "checkouts_equal": git_total - len(git_unequal), "checkouts_unequal": git_unequal,
        "replaced": {**replaced, "equal": replaced["total"] - len(replaced["unequal"])},
        "vendored": vendor, "auth": auth, "dirs_equal": dirs_equal, "blaze": run.blaze, "seconds": run.seconds,
        "unequal": unequal, "failed": run.failed,
    }
    with open(os.path.join(work, "fetchdiff.json"), "w") as f:
        json.dump(result, f, indent=1)
    print(f"{len(locks)} lock files, {len(run.failed)} not fetched (fetchdiff.json says why)")
    print(f"{total - len(unequal)}/{total} unpacked packages equal ({len(registry)} registry packages in the "
          f"lock files)")
    print(f"{git_total - len(git_unequal)}/{git_total} git checkouts equal ({len(git)} git packages in the lock files)")
    print(f"crates.io replaced by a mirror: {result['replaced']['equal']}/{replaced['total']} equal")
    for client, it in auth.items():
        print(f"registry that requires a token, {client}: {it['asked']} requests, {it['with_token']} with the token, "
              f"{it['crates']} .crate files, {'as cargo sends it' if it['as_cargo_sends'] else 'NOT as cargo sends it'}")
    for name, (count, equal) in vendor.items():
        print(f"vendored {name}: {equal}/{count} read where cargo vendor put them, every file checked")
    for part, (a, b) in dirs.items():
        name = part if part.startswith("git/") else f"registry/{part}"
        print(f"{name}: {'equal' if a == b else 'cargo ' + ', '.join(a) + '; blaze ' + ', '.join(b)}")
    print(f"cargo {run.seconds['cargo']:.1f} s, blaze {run.seconds['blaze']:.1f} s")
    for package, found in [*unequal.items(), *git_unequal.items(), *replaced["unequal"].items()][:20]:
        print(f"  {package}: {found[0]}" + (f" (and {len(found) - 1} more)" if len(found) > 1 else ""))
    bad = unequal or git_unequal or replaced["unequal"] or not dirs_equal or any(c != e for c, e in vendor.values())
    bad = bad or len(auth) != 2 or not all(it["as_cargo_sends"] and it["crates"] for it in auth.values())
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
