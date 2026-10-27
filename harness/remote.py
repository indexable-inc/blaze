#!/usr/bin/env python3
"""remote.py: a store shared by two machines through an S3 service, with the service at fault.

Machine A builds a workspace, which sends its store to a bucket. Machine B, with another HOME,
another checkout and an empty store, fetches it and builds: it has to compile nothing and
print what cargo's build prints. Then the transfer is broken in every way the proxy of
s3fake.py knows, the objects are spoiled, and the lists are forged; each time B has to end
with either the pack as A sent it or no pack, and a build that prints the same.

It runs against MinIO and SeaweedFS as it finds them (`RA_JIT_MINIO`, `RA_JIT_WEED`, PATH,
`~/s3tools`). With `RA_JIT_S3_REMOTE=s3://bucket/prefix` it runs against that instead, with
the credentials the AWS tools would use from the environment, and leaves out what needs the
proxy or needs to kill the service.

    python3 remote.py                              the checks
    python3 remote.py --measure WORKSPACE -p PACKAGE --bin NAME --crate A,B,C [--run ARG]
                                                   a fresh machine with and without the remote
"""
import argparse, hashlib, os, re, shlex, shutil, signal, subprocess, sys, tempfile, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import linkline
from difftest import RA_JIT, RUNNER, TARGET_ARGS, linker_line, sysroot_rlibs
from s3fake import ACCESS, SECRET, Bucket, Proxy, Server, find

from packs import FORMAT, sealed as pack_of, records as records_of

MIB = 1 << 20
# Enough for three parts of a multipart upload.
BIG = 20 * MIB
REAL = os.environ.get("RA_JIT_S3_REMOTE")
# An object the harness keeps in a bucket of its own to see that the service reads again.
PROBE = "harness-probe"
# The longest a service may be away, in seconds, for a fetch to have to ride it out.
RIDDEN_OUT = 10

right = total = 0
def check(what, ok, detail=""):
    global right, total
    total += 1; right += bool(ok)
    print(f"  {what:66} {'right' if ok else 'WRONG'} {detail}", flush=True)
    return ok


def digest(data):
    return hashlib.sha256(data).hexdigest()


def packed(data):
    """`data` compressed the way ra-jit sends a pack."""
    return subprocess.run(["zstd", "-3", "-c"], input=data, capture_output=True, check=True).stdout


def unpacked(data):
    """What a compressed object holds, or nothing when it is not one."""
    return subprocess.run(["zstd", "-dc"], input=data or b"", capture_output=True).stdout


def filler(seed, size):
    """A sealed pack of `size` bytes that no build asks anything of."""
    # Bytes that do not compress, so that what goes over the line is as large as the pack.
    value = hashlib.shake_256(seed.encode()).digest(size - len(pack_of([(bytes(32), b"")])))
    return pack_of([(hashlib.sha256(b"key" + seed.encode()).digest(), value)])


class Remote:
    """One service and the bucket on it, as ra-jit is pointed at it and as the harness sees it."""
    def __init__(self, name, server, top):
        self.name, self.server, self.count = name, server, 0
        self.keys = os.path.join(top, name + "-keys")
        os.makedirs(self.keys)
        if server:
            self.proxy = Proxy(server.port)
            self.endpoint = f"http://127.0.0.1:{self.proxy.port}"
            self.bucket_name, self.base = "ra-jit-harness", ""
            self.env = {"AWS_ACCESS_KEY_ID": ACCESS, "AWS_SECRET_ACCESS_KEY": SECRET, "AWS_REGION": "us-east-1",
                        "AWS_EC2_METADATA_DISABLED": "true"}
            self.secrets = [SECRET]
        else:
            self.proxy = None
            rest = REAL[len("s3://"):].split("?")[0]
            self.bucket_name, _, base = rest.partition("/")
            self.base = base.strip("/") + "/" if base.strip("/") else ""
            self.region = os.environ.get("AWS_REGION") or os.environ.get("AWS_DEFAULT_REGION") or "us-east-1"
            self.endpoint = os.environ.get("AWS_ENDPOINT_URL") or f"https://s3.{self.region}.amazonaws.com"
            self.env = {}
            self.secrets = [os.environ[it] for it in ("AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN") if it in os.environ]

    def start(self):
        if self.server:
            self.server.start()
            self.direct().create()
            self.direct().put(PROBE, PROBE.encode())

    def direct(self, prefix=""):
        """The bucket as the service has it, past the proxy."""
        return Bucket(f"http://127.0.0.1:{self.server.port}", self.bucket_name, prefix)

    def restart(self):
        """Starts the service again and waits until it has its objects again: one that was just
        killed lists its bucket some seconds before it can read from it."""
        self.server.start()
        end = time.time() + 60
        while self.direct().get(PROBE) != PROBE.encode():
            if time.time() > end:
                raise SystemExit(f"{self.name} does not answer after it was started again")
            time.sleep(0.1)

    def fresh(self):
        """A prefix nothing is under yet: the address for ra-jit and the bucket for the harness."""
        self.count += 1
        prefix = f"{self.base}run-{os.getpid()}-{self.count}/"
        if self.server:
            return (f"s3://{self.bucket_name}/{prefix}?endpoint={self.endpoint}",
                    self.direct(prefix))
        query = REAL.partition("?")[2]
        bucket = Bucket(self.endpoint, self.bucket_name, prefix, os.environ.get("AWS_ACCESS_KEY_ID", ""),
                        os.environ.get("AWS_SECRET_ACCESS_KEY", ""), self.region, os.environ.get("AWS_SESSION_TOKEN"))
        return f"s3://{self.bucket_name}/{prefix}" + ("?" + query if query else ""), bucket


# How a daemon is told to end, and how it ends then. On Windows that is Ctrl-Break, which
# reaches one process group, and the status of a process ended by its console.
if os.name == "nt":
    OWN_GROUP = dict(creationflags=subprocess.CREATE_NEW_PROCESS_GROUP)
    TOLD_TO_END = 0xC000013A
    tell_to_end = lambda process: process.send_signal(signal.CTRL_BREAK_EVENT)
else:
    OWN_GROUP = {}
    TOLD_TO_END = -signal.SIGTERM
    tell_to_end = lambda process: process.terminate()


class Machine:
    """A HOME, a store and a checkout of its own. The toolchain is the one this machine has:
    what a fresh machine downloads of it is not what is measured here."""
    def __init__(self, top, name, remote):
        self.dir = os.path.join(top, name)
        self.home, self.store, self.out = (os.path.join(self.dir, it) for it in ("home", "store", "out"))
        for it in (self.home, self.out):
            os.makedirs(it)
        home = os.path.expanduser("~")
        self.env = {**{k: v for k, v in os.environ.items() if not k.startswith(("AWS_", "RA_JIT_REMOTE", "RA_JIT_CACHE"))},
                    "HOME": self.home, "CARGO_HOME": os.environ.get("CARGO_HOME", f"{home}/.cargo"),
                    "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", f"{home}/.rustup"), "RA_JIT_CACHE": self.store,
                    # Most of what is checked here is the transfer, between machines that name no keys.
                    "RA_JIT_REMOTE_UNSIGNED": "1",
                    **({k: os.environ[k] for k in os.environ if k.startswith("AWS_")} if not remote.server else {}),
                    **remote.env}
        self.said = ""

    def ra_jit(self, *args, env=None, cwd=None, timeout=300):
        out = subprocess.run([RA_JIT, *args], capture_output=True, text=True, env={**self.env, **(env or {})}, cwd=cwd,
                             timeout=timeout)
        self.said += out.stdout + out.stderr
        return out

    def packs(self):
        """The sealed packs of the store, and whether each is what its name says."""
        if not os.path.isdir(self.store):
            return {}
        return {name: digest(open(os.path.join(self.store, name), "rb").read()) == name[:-len(".pack")]
                for name in os.listdir(self.store) if name.endswith(".pack")}

    def partials(self):
        return [name for name in os.listdir(self.store) if name.endswith(".tmp")] if os.path.isdir(self.store) else []

    def add_pack(self, data):
        """Puts a sealed pack in the store, as a daemon of this format would have. Returns its name."""
        os.makedirs(self.store, exist_ok=True)
        open(os.path.join(self.store, "version"), "w").write(f"ra-jit store {FORMAT}\n")
        name = digest(data) + ".pack"
        open(os.path.join(self.store, name), "wb").write(data)
        return name

    def cache(self, verb, *args, env=None):
        """`ra-jit cache stats|verify|gc`: (whether it ended well, its `name: number` lines, what it printed)."""
        out = self.ra_jit("cache", verb, *args, env=env)
        numbers = {}
        for line in out.stdout.splitlines():
            name, _, value = line.partition(":")
            if value.split() and value.split()[0].isdigit():
                numbers[name.strip()] = int(value.split()[0])
        return out.returncode == 0, numbers, out.stdout + out.stderr

    def empty(self):
        shutil.rmtree(self.store, ignore_errors=True)

    def checkout(self, workspace):
        self.ws = os.path.join(self.dir, "checkout")
        top = os.path.realpath(workspace)
        built = lambda at, names: [n for n in names if n == ".git" or (n == "target" and os.path.realpath(at) == top)]
        shutil.copytree(workspace, self.ws, ignore=built)

    def cargo(self, target):
        """Builds with cargo and keeps the line it links with. Returns the seconds it took."""
        start = time.time()
        out = subprocess.run(["cargo", "rustc", *target, *TARGET_ARGS, "--", "--print", "link-args", "-C", "save-temps"],
                             cwd=self.ws, capture_output=True, text=True, env=self.env)
        if out.returncode != 0:
            raise SystemExit(f"cargo failed:\n{out.stderr[-3000:]}")
        self.link_args = [a for a in linker_line(out.stdout + out.stderr) if not a.endswith("symbols.o")]
        self.reference = linkline.output_of(self.link_args)
        return time.time() - start

    def build(self, crates, env=None):
        """Has ra-jit compile the crates and link the program. Returns what ra-jit said and the seconds."""
        rlibs = [a for a in self.link_args if a.endswith((".rlib", ".rcgu.o"))]
        symbols = [x for rlib in rlibs + sysroot_rlibs() for x in ("--symbols", rlib)]
        obj, self.program = os.path.join(self.out, "ra-jit.o"), os.path.join(self.out, "program")
        link = linkline.with_output(linkline.ahead(list(self.link_args), [obj], RA_JIT), self.program)
        start = time.time()
        out = self.ra_jit(self.ws, *TARGET_ARGS, *(x for c in crates for x in ("--crate", c)), "--out", obj,
                         "--link-cmd", linkline.join(link), *symbols, env=env, timeout=3600)
        if out.returncode != 0:
            raise SystemExit(f"ra-jit failed:\n{out.stderr[-3000:]}")
        return out.stderr, time.time() - start

    def daemon(self, crates, env):
        """Starts a daemon for the program and waits for its first build. Returns it and where it listens."""
        rlibs = [a for a in self.link_args if a.endswith((".rlib", ".rcgu.o"))]
        symbols = [x for rlib in rlibs + sysroot_rlibs() for x in ("--symbols", rlib)]
        obj, self.program = os.path.join(self.out, "ra-jit.o"), os.path.join(self.out, "program")
        link = linkline.with_output(linkline.ahead(list(self.link_args), [obj], RA_JIT), self.program)
        self.count = getattr(self, "count", 0) + 1
        socket, self.log = os.path.join(self.out, f"socket{self.count}"), os.path.join(self.out, f"daemon{self.count}.log")
        daemon = subprocess.Popen(
            [RA_JIT, self.ws, *TARGET_ARGS, *(x for c in crates for x in ("--crate", c)), "--out", obj, "--socket", socket,
             "--link-cmd", linkline.join(link), *symbols], stdout=open(self.log, "w"), stderr=subprocess.STDOUT,
            env={**self.env, **env}, **OWN_GROUP)
        while "watching" not in self.heard():
            if daemon.poll() is not None:
                raise SystemExit(f"the daemon did not start: {self.log}")
            time.sleep(0.05)
        return daemon, socket

    def heard(self):
        return open(self.log).read()

    def edit(self, socket, note):
        """Changes `main` and has the daemon build it. Returns how long the daemon took, if it built."""
        path = os.path.join(self.ws, "app/src/main.rs")
        text = open(path).read()
        open(path, "w").write(text.replace("fn main() {", f'fn main() {{\n    println!("{note}");', 1))
        start = time.time()
        asked = subprocess.run([RA_JIT, "sync", socket], capture_output=True, text=True, timeout=120, env=self.env)
        return time.time() - start if asked.returncode == 0 and note in self.run() else None

    def run(self, program=None, args=()):
        ran = subprocess.run([*RUNNER, program or self.program, *args], capture_output=True, text=True, timeout=60)
        return ran.stdout + (f"[exit {ran.returncode}]" if ran.returncode else "")


def said_number(text, before):
    """The number that stands before `before` in what ra-jit said, the last time it said so."""
    import re
    found = re.findall(r"(\d+) " + re.escape(before), text)
    return int(found[-1]) if found else None


def compiled(log):
    return said_number(log, "compiled and added")


def round_trip(top, remote, name, crates, target, workspace):
    """A builds and sends, B fetches and builds without compiling."""
    print(f"{remote.name}: the round trip", flush=True)
    address, bucket = remote.fresh()
    a, b = Machine(top, f"{name}-a", remote), Machine(top, f"{name}-b", remote)
    for machine in (a, b):
        machine.checkout(workspace)
        machine.cargo(target)
    log, _ = a.build(crates, env={"RA_JIT_REMOTE": address})
    expected = a.run(a.reference)
    check("A compiles and its program prints what cargo's does", compiled(log) and a.run() == expected, f"{compiled(log)} compiled")
    # A store too small to seal is sent by the next thing that finds it left open.
    out = a.ra_jit("cache", "push", address)
    objects = bucket.objects()
    packs = [it for it in objects if it.startswith("packs/")]
    check("the bucket has A's packs, their keys and the manifest",
          out.returncode == 0 and packs and sorted("packs/" + it for it in a.packs()) == sorted(it.removesuffix(".zst") for it in packs)
          and "manifests/default" in objects and len([it for it in objects if it.startswith("keys/")]) == len(packs),
          f"{len(objects)} objects")
    check("every object in the bucket is what its name says",
          all(digest(unpacked(bucket.get(it)) if it.endswith(".zst") else bucket.get(it)) == it.split("/")[1].split(".")[0]
              for it in objects if not it.startswith("manifests/")))
    out = a.ra_jit("cache", "push", address)
    check("a second push sends nothing", out.returncode == 0 and out.stdout.startswith("0 packs added"), out.stdout.strip())

    log, _ = b.build(crates, env={"RA_JIT_REMOTE": address})
    check("B fetches and compiles nothing", compiled(log) == 0 and all(b.packs().values()) and set(a.packs()) <= set(b.packs()),
          f"{said_number(log, 'functions found')} found, {compiled(log)} compiled")
    check("B's program prints what A's and cargo's do", b.run() == expected == b.run(b.reference))
    whole, numbers, said = b.cache("verify")
    check("B's store is whole after the pull and the build", whole and numbers.get("packs") == len(b.packs())
          and numbers.get("corrupt packs") == 0 and not b.partials(), f"{numbers.get('records')} records")
    return a, b, address, bucket, expected


def configuration(top, remote, a, address, bucket):
    """Where the credentials and the address come from, and a machine that only reads."""
    print(f"{remote.name}: credentials, addressing, reading only", flush=True)
    c = Machine(top, f"config-{remote.name}", remote)
    before = bucket.objects()
    out = a.ra_jit("cache", "push", address, "other", env={"RA_JIT_REMOTE_READONLY": "1"})
    check("a machine that only reads refuses to push and says why",
          out.returncode != 0 and "RA_JIT_REMOTE_READONLY" in out.stderr and bucket.objects() == before, out.stderr.strip()[-80:])
    out = c.ra_jit("cache", "pull", address, env={"RA_JIT_REMOTE_READONLY": "1"})
    check("and pulls", out.returncode == 0 and all(c.packs().values()) and c.packs(), out.stdout.strip())
    if not remote.server:
        return
    shared = os.path.join(c.home, ".aws")
    os.makedirs(shared)
    open(os.path.join(shared, "credentials"), "w").write(
        f"[default]\naws_access_key_id = nobody\naws_secret_access_key = nothing\n"
        f"[harness]\naws_access_key_id = {ACCESS}\naws_secret_access_key = {SECRET}\n")
    open(os.path.join(shared, "config"), "w").write(
        f"[profile harness]\nregion = us-east-1\nendpoint_url = {remote.endpoint}\n")
    without = {k: v for k, v in c.env.items() if not k.startswith("AWS_")}
    plain = address.split("?")[0]
    c.empty()
    out = subprocess.run([RA_JIT, "cache", "pull", plain + "?profile=harness"], capture_output=True, text=True, env=without)
    check("credentials, region and endpoint from a profile of the shared files", out.returncode == 0 and c.packs(),
          (out.stdout + out.stderr).strip()[-80:])
    c.empty()
    out = subprocess.run([RA_JIT, "cache", "pull", plain], capture_output=True, text=True, env={**without, "AWS_PROFILE": "harness"})
    check("the same with AWS_PROFILE", out.returncode == 0 and c.packs(), (out.stdout + out.stderr).strip()[-80:])
    c.empty()
    out = subprocess.run([RA_JIT, "cache", "pull", plain], capture_output=True, text=True,
                         env={**c.env, "AWS_ENDPOINT_URL": remote.endpoint})
    check("the endpoint from AWS_ENDPOINT_URL", out.returncode == 0 and c.packs(), (out.stdout + out.stderr).strip()[-80:])
    c.empty()
    out = c.ra_jit("cache", "pull", address + "&region=eu-west-1&path-style=true")
    check("another region and addressing by path asked for", out.returncode == 0 and c.packs(), out.stderr.strip()[-80:])
    if remote.server.kind == "minio" and os.name == "nt":
        # glibc and macOS answer for every name under `localhost`; Windows asks the network.
        print("  the bucket in the name of the host: left out, Windows resolves no name under `localhost`", flush=True)
    elif remote.server.kind == "minio":
        c.empty()
        remote.proxy.clear()
        by_host = address.replace("127.0.0.1", "localhost") + "&path-style=false"
        out = c.ra_jit("cache", "pull", by_host)
        asked = remote.proxy.seen("GET", "/packs/")
        check("the bucket in the name of the host",
              out.returncode == 0 and c.packs() and asked and not any(remote.bucket_name in it[1] for it in asked),
              (out.stdout + out.stderr).strip()[-80:])
    c.empty()
    out = c.ra_jit("cache", "pull", address, env={"AWS_SECRET_ACCESS_KEY": "wrong-" + SECRET[::-1]})
    check("the wrong secret is refused by the service, and said", out.returncode != 0 and not c.packs(), out.stderr.strip()[-60:])
    out = c.ra_jit("cache", "pull", address + "&nonsense=1")
    check("an option that is none is refused", out.returncode != 0 and "nonsense" in out.stderr)
    everything = c.said + a.said
    check("no secret in anything ra-jit printed", not any(it in everything or it[::-1] in everything for it in remote.secrets))


def big_store(machine, seed):
    """A store with one pack big enough to go up in parts. Returns its name."""
    return machine.add_pack(filler(seed, BIG))


def transfers(top, remote):
    """Every way the proxy can break a transfer, and the service killed under it."""
    print(f"{remote.name}: transfers that break", flush=True)
    proxy, counter = remote.proxy, [0]
    quick = {"RA_JIT_REMOTE_TIMEOUT": "2"}

    def pair(what):
        counter[0] += 1
        address, bucket = remote.fresh()
        a, b = (Machine(top, f"t{counter[0]}-{remote.name}-{it}", remote) for it in "ab")
        name = big_store(a, f"{remote.name} {what} {counter[0]}")
        proxy.clear()
        return a, b, address, bucket, name

    def pushed(what):
        a, b, address, bucket, name = pair(what)
        out = a.ra_jit("cache", "push", address)
        if out.returncode != 0:
            raise SystemExit(f"the push before `{what}` failed: {out.stderr}")
        proxy.clear()
        return a, b, address, bucket, name

    def whole(machine, name):
        return machine.packs() == {name: True} and not machine.partials()

    a, b, address, bucket, name = pair("parts")
    out = a.ra_jit("cache", "push", address)
    parts = proxy.seen("PUT", "partNumber=")
    check("a big pack goes up in parts, a few at a time",
          out.returncode == 0 and len(parts) == 3 and proxy.seen("POST", r"uploads") and proxy.seen("POST", "uploadId=")
          and digest(bucket.get("packs/" + name)) == name[:-5] and not bucket.uploads(), f"{len(parts)} parts")
    check("ra-jit counts the requests the proxy saw", said_number(out.stderr, "requests") == len(proxy.requests),
          f"{said_number(out.stderr, 'requests')} and {len(proxy.requests)}")
    proxy.clear()
    out = b.ra_jit("cache", "pull", address)
    check("and comes down whole", out.returncode == 0 and whole(b, name) and said_number(out.stderr, "bytes received") >= BIG)
    check("in the requests the proxy saw", said_number(out.stderr, "requests") == len(proxy.requests),
          f"{said_number(out.stderr, 'requests')} and {len(proxy.requests)}")

    a, b, address, bucket, name = pushed("503 down")
    fault = proxy.fault("GET", "/packs/", times=2, status=503)
    out = b.ra_jit("cache", "pull", address)
    check("503 twice on a fetch: asked again", out.returncode == 0 and whole(b, name) and fault.hits == 2
          and said_number(out.stderr, "tried again") >= 2, out.stderr.strip().split("\n")[0][-70:])

    a, b, address, bucket, name = pair("503 up")
    faults = [proxy.fault("PUT", "partNumber=2", times=2, status=500), proxy.fault("POST", "uploadId=", times=1, status=503),
              proxy.fault("PUT", "/manifests/", times=1, status=503)]
    out = a.ra_jit("cache", "push", address)
    check("5xx on a part, on completing and on the manifest: sent again",
          out.returncode == 0 and [it.hits for it in faults] == [2, 1, 1] and digest(bucket.get("packs/" + name)) == name[:-5])

    a, b, address, bucket, name = pushed("always 503")
    proxy.fault("GET", "/packs/", times=1000, status=503)
    start = time.time()
    out = b.ra_jit("cache", "pull", address)
    check("503 every time: given up, said, nothing left as a pack", out.returncode != 0 and not b.packs() and "503" in out.stderr,
          f"{time.time() - start:.1f}s, {len(proxy.requests)} requests")

    a, b, address, bucket, name = pushed("cut down")
    fault = proxy.fault("GET", "/packs/", times=2, cut=5 * MIB)
    out = b.ra_jit("cache", "pull", address)
    ranges = [it[2] for it in proxy.seen("GET", "/packs/")]
    check("the connection dropped twice in a fetch: taken up where it stopped",
          out.returncode == 0 and whole(b, name) and said_number(out.stderr, "transfers resumed") == 2
          and ranges[0] is None and all(ranges[1:]) and said_number(out.stderr, "bytes received") < BIG + MIB, f"{ranges}")

    a, b, address, bucket, name = pushed("reset")
    fault = proxy.fault("GET", "/packs/", times=2, reset=True)
    out = b.ra_jit("cache", "pull", address)
    check("the connection closed with no answer, twice", out.returncode == 0 and whole(b, name) and fault.hits == 2)

    a, b, address, bucket, name = pair("cut up")
    fault = proxy.fault("PUT", "partNumber=1", times=1, cut_request=2 * MIB)
    out = a.ra_jit("cache", "push", address)
    check("the connection dropped in the middle of a part: sent again",
          out.returncode == 0 and fault.hits == 1 and digest(bucket.get("packs/" + name)) == name[:-5] and not bucket.uploads())

    a, b, address, bucket, name = pushed("stall")
    fault = proxy.fault("GET", "/packs/", times=1, stall=(3 * MIB, 6))
    start = time.time()
    out = b.ra_jit("cache", "pull", address, env=quick)
    check("a fetch that stops for 6s with 2s of patience: taken up",
          out.returncode == 0 and whole(b, name) and said_number(out.stderr, "transfers resumed") >= 1 and time.time() - start < 6,
          f"{time.time() - start:.1f}s")

    a, b, address, bucket, name = pushed("slow answer")
    proxy.hold = 4
    start = time.time()
    out = b.ra_jit("cache", "pull", address, env={"RA_JIT_REMOTE_TIMEOUT": "1", "AWS_MAX_ATTEMPTS": "2"})
    check("a service that takes 4s to answer with 1s of patience: given up, said",
          out.returncode != 0 and not b.packs() and time.time() - start < 45, f"{time.time() - start:.1f}s {out.stderr.strip()[-50:]}")
    proxy.hold = 0

    a, b, address, bucket, name = pushed("ra-jit killed")
    proxy.rate = 4 * MIB
    process = subprocess.Popen([RA_JIT, "cache", "pull", address], env=b.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(2)
    process.kill()
    process.wait()
    left = b.partials()
    size = os.path.getsize(os.path.join(b.store, left[0])) if left else 0
    check("ra-jit killed in a fetch leaves no pack", not b.packs() and len(left) == 1 and 0 < size < BIG, f"{size} bytes kept aside")
    whole_store, _, _ = b.cache("verify")
    counted = b.cache("stats")[1]
    check("the store is whole, and counts what was kept aside as a temporary file",
          whole_store and counted.get("packs") == 0 and counted.get("temporary files") == 1 and counted.get("temporary bytes") == size)
    proxy.rate = None
    # Old enough for a collection to take any other temporary file, an hour after its last byte.
    aside = os.path.join(b.store, left[0]) if left else None
    if aside:
        os.utime(aside, (time.time() - 3600,) * 2)
    b.cache("gc")
    check("a collection an hour later leaves what was kept aside", b.partials() == left and aside
          and os.path.getsize(aside) == size)
    out = b.ra_jit("cache", "pull", address)
    check("the next one fetches the rest only", out.returncode == 0 and whole(b, name)
          and said_number(out.stderr, "bytes received") < BIG - size + MIB, out.stderr.strip().split("\n")[0][-60:])

    a, b, address, bucket, name = pushed("spoiled partial")
    b.add_pack(filler(f"{remote.name} there already", 4096))
    open(os.path.join(b.store, name + ".fetch.tmp"), "wb").write(b"what a stopped process left of something else" * 1000)
    out = b.ra_jit("cache", "pull", address)
    check("a part kept aside that is of something else: fetched from the start",
          out.returncode == 0 and b.packs().get(name) is True and not b.partials()
          and said_number(out.stderr, "objects rejected") == 0)

    a, b, address, bucket, name = pushed("two at once")
    proxy.rate = 8 * MIB
    both = [subprocess.Popen([RA_JIT, "cache", "pull", address], env=b.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            for _ in range(2)]
    ends = [it.communicate() for it in both]
    check("two processes fetching into one store: one fetches, both end well",
          all(it.returncode == 0 for it in both) and whole(b, name) and len(proxy.seen("GET", "/packs/")) == 1)
    proxy.rate = None

    counter[0] += 1
    address, bucket = remote.fresh()
    a, b = (Machine(top, f"t{counter[0]}-{remote.name}-{it}", remote) for it in "ab")
    for at in range(12):
        a.add_pack(filler(f"{remote.name} small {at}", 64 << 10))
    proxy.clear()
    proxy.hold = 0.05
    out = a.ra_jit("cache", "push", address, env={"RA_JIT_REMOTE_JOBS": "3"})
    most = proxy.most_flying
    proxy.most_flying = 0
    out2 = b.ra_jit("cache", "pull", address, env={"RA_JIT_REMOTE_JOBS": "3"})
    check("twelve packs with three requests at a time: never more in flight",
          out.returncode == 0 and out2.returncode == 0 and 2 <= most <= 3 and 2 <= proxy.most_flying <= 3 and len(b.packs()) == 12
          and all(b.packs().values()),
          f"{most} up, {proxy.most_flying} down, {len(b.packs())} packs, ended {out.returncode} and {out2.returncode}"
          f"{': ' + out2.stderr.strip().splitlines()[0][-80:] if out2.returncode and out2.stderr.strip() else ''}")
    proxy.hold = 0

    a, b, address, bucket, name = pushed("server killed and back")
    proxy.rate = 4 * MIB
    down = []
    def bounce():
        time.sleep(1.5)
        remote.server.kill()
        start = time.time()
        time.sleep(1)
        remote.restart()
        down.append(time.time() - start)
    bouncing = threading.Thread(target=bounce)
    bouncing.start()
    out = b.ra_jit("cache", "pull", address)
    bouncing.join()
    # A service is back when it reads again, which is seconds after it listens again and is
    # not the same every time. ra-jit waits about 16s for a byte; an outage well under that
    # has to be ridden out, and a longer one has to end as a service that is gone does.
    if down[0] < RIDDEN_OUT or out.returncode == 0:
        taken_up = out.returncode == 0 and whole(b, name) and said_number(out.stderr, "transfers resumed") >= 1
    else:
        proxy.rate = None
        taken_up = out.returncode != 0 and not b.packs() and b.ra_jit("cache", "pull", address).returncode == 0 and whole(b, name)
    check("the service killed in a fetch and back: taken up", taken_up,
          f"reading again after {down[0]:.1f}s; " + out.stderr.strip().split("\n")[0][-90:])

    a, b, address, bucket, name = pushed("server killed")
    proxy.rate = 4 * MIB
    killing = threading.Timer(1.5, remote.server.kill)
    killing.start()
    start = time.time()
    out = b.ra_jit("cache", "pull", address)
    killing.join()
    left = b.partials()
    check("the service killed in a fetch and gone: given up, said, no pack",
          out.returncode != 0 and not b.packs() and len(left) == 1 and "fetching packs/" in out.stderr, f"{time.time() - start:.1f}s")
    remote.restart()
    proxy.rate = None
    out = b.ra_jit("cache", "pull", address)
    check("back: the rest is fetched", out.returncode == 0 and whole(b, name) and said_number(out.stderr, "bytes received") < BIG,
          out.stderr.strip()[-200:])

    a, b, address, bucket, name = pair("server killed in a push")
    proxy.rate = 4 * MIB
    killing = threading.Timer(1.5, remote.server.kill)
    killing.start()
    out = a.ra_jit("cache", "push", address, env={"AWS_MAX_ATTEMPTS": "2"})
    killing.join()
    remote.restart()
    proxy.rate = None
    objects = bucket.objects()
    check("the service killed in a push: said, no pack and no manifest in the bucket",
          out.returncode != 0 and "packs/" + name not in objects and "manifests/default" not in objects, out.stderr.strip()[-60:])
    out = a.ra_jit("cache", "push", address)
    check("back: the push goes through and no upload is left unfinished",
          out.returncode == 0 and digest(bucket.get("packs/" + name)) == name[:-5] and not bucket.uploads(),
          out.stdout.strip() or out.stderr.strip()[-200:])


def stored(top, remote):
    """What is fetched is the store's: counted, kept under the limit, evicted, fetched again."""
    print(f"{remote.name}: the store a pull fills", flush=True)
    address, bucket = remote.fresh()
    a, b, c = (Machine(top, f"k-{remote.name}-{it}", remote) for it in "abc")
    sent = [a.add_pack(filler(f"{remote.name} kept {at}", MIB)) for at in range(6)]
    if a.ra_jit("cache", "push", address).returncode != 0:
        raise SystemExit("the push failed")

    out = b.ra_jit("cache", "pull", address)
    whole, verified, _ = b.cache("verify")
    _, counted, _ = b.cache("stats")
    check("after a pull verify and stats count what was fetched", out.returncode == 0 and whole
          and verified.get("packs") == 6 and verified.get("records") == 6 and verified.get("bytes") == 6 * MIB
          and counted.get("packs") == 6 and counted.get("pack bytes") == 6 * MIB and counted.get("temporary files") == 0,
          f"{counted.get('pack bytes')} bytes")

    ok, collected, _ = b.cache("gc", "--max", "0")
    out = b.ra_jit("cache", "pull", address)
    whole, verified, _ = b.cache("verify")
    check("packs that were evicted are fetched again", ok and collected.get("evicted packs") == 6 and out.returncode == 0
          and whole and sorted(b.packs()) == sorted(sent) and said_number(out.stderr, "bytes received") > 6 * MIB)

    limit = {"RA_JIT_CACHE_MAX": str(3 * MIB + 4096)}
    packs_fetched(remote)
    out = c.ra_jit("cache", "pull", address, env=limit)
    _, counted, _ = c.cache("stats", env=limit)
    whole, _, _ = c.cache("verify")
    check("a pull into a store with a limit fetches what fits and says what does not",
          out.returncode == 0 and "3 left that do not fit under RA_JIT_CACHE_MAX" in out.stdout and whole
          and counted.get("packs") == 3 and counted.get("pack bytes") <= 3 * MIB + 4096
          and packs_fetched(remote) in (3, None), out.stdout.strip()[:90])

    # What was there before is the oldest, and makes room for what is fetched.
    d = Machine(top, f"k-{remote.name}-d", remote)
    old = [d.add_pack(filler(f"{remote.name} old {at}", MIB)) for at in range(2)]
    for name in old:
        os.utime(os.path.join(d.store, name), (time.time() - 3600, time.time() - 3600))
    out = d.ra_jit("cache", "pull", address, env=limit)
    _, counted, _ = d.cache("stats", env=limit)
    whole, _, _ = d.cache("verify")
    check("what is fetched counts toward the limit: the packs unused the longest go",
          out.returncode == 0 and "2 packs unused the longest evicted" in out.stdout and whole and counted.get("packs") == 3
          and not set(old) & set(d.packs()) and set(d.packs()) <= set(sent), out.stdout.strip()[-80:])

    # A store makes one pack of many small ones; what was fetched is then there under another name.
    address, bucket = remote.fresh()
    f, g = (Machine(top, f"k-{remote.name}-{it}", remote) for it in "fg")
    small = [f.add_pack(filler(f"{remote.name} small kept {at}", 64 << 10)) for at in range(24)]
    f.ra_jit("cache", "push", address)
    first = g.ra_jit("cache", "pull", address)
    ok, collected, _ = g.cache("gc")
    packs_fetched(remote)
    out = g.ra_jit("cache", "pull", address)
    whole, verified, _ = g.cache("verify")
    check("packs the store made one of are not fetched again", first.returncode == 0 and ok and len(g.packs()) < 24
          and out.returncode == 0 and out.stdout.startswith("0 packs") and "in other packs of the store passed over" in out.stdout
          and packs_fetched(remote) in (0, None) and whole and verified.get("records") == 24,
          f"{out.stdout.strip()[:100]}; first exit {first.returncode}, {first.stdout.strip()[:120]} {first.stderr.strip()[-600:]}; merged {collected.get('merged packs')}, "
          f"{len(g.packs())} packs, {verified.get('records')} records; {out.stderr.strip()[:300]}")

    # A pack of the format before this one, listed as what it is and listed as what it is not.
    address, bucket = remote.fresh()
    other = b"rajitpk" + bytes([FORMAT - 1]) + b" what the format before wrote"
    keys = b"\0" * 32
    bucket.put("packs/" + digest(other) + ".pack", other)
    bucket.put("keys/" + digest(keys), keys)
    e = Machine(top, f"k-{remote.name}-e", remote)
    packs_fetched(remote)
    for said_format, what in ((FORMAT - 1, "left"), (FORMAT, "rejected")):
        bucket.put("manifests/default", f"{digest(other)} {len(other)} {digest(keys)} {said_format}\n".encode())
        out = e.ra_jit("cache", "pull", address)
        whole, verified, _ = e.cache("verify")
        left = out.returncode == 0 and "1 of another format left" in out.stdout and packs_fetched(remote) in (0, None)
        rejected = out.returncode != 0 and "is not what it is listed as" in out.stderr and said_number(out.stderr, "objects rejected") == 1
        check(f"a pack of another format, listed as format {said_format}: {what}",
              (left if what == "left" else rejected) and whole and not e.packs() and not e.partials(),
              (out.stdout + out.stderr).strip()[-70:])


def collected(top, remote):
    """`ra-jit cache gc --remote`: what no list names goes when it is old, and nothing else does."""
    print(f"{remote.name}: the remote collected", flush=True)
    DAYS = 24 * 3600

    def gc(machine, address, *args, env=None):
        out = machine.ra_jit("cache", "gc", "--remote", address, *args, env=env)
        numbers = {}
        for line in out.stdout.splitlines():
            name, _, value = line.partition(":")
            numbers[name.strip()] = [int(it) for it in re.findall(r"\d+", value)]
        return out, numbers

    def aged(seconds, one=True):
        remote.proxy.clear()
        remote.proxy.age, remote.proxy.age_one = seconds, one

    address, bucket = remote.fresh()
    a, b = (Machine(top, f"g-{remote.name}-{it}", remote) for it in "ab")
    sent = [a.add_pack(filler(f"{remote.name} collected {at}", 256 << 10)) for at in range(3)]
    if a.ra_jit("cache", "push", address).returncode != 0:
        raise SystemExit("the push failed")
    stray = filler(f"{remote.name} listed nowhere", 256 << 10)
    strays = {f"packs/{digest(stray)}.pack.zst": packed(stray), f"keys/{digest(b'k' * 32)}": b"k" * 32,
              f"units/{digest(b'a unit')}": b"a unit"}
    for name, data in strays.items():
        bucket.put(name, data)
    before = bucket.objects()

    out, said = gc(a, address)
    check("what was just written stays, listed or not", out.returncode == 0 and said.get("objects removed") == [0]
          and said.get("lines taken out") == [0, 0] and bucket.objects() == before, out.stdout.splitlines()[0] if out.stdout else out.stderr[-80:])
    out, _ = gc(a, address, "--unlisted-after", "1h")
    check("an unlisted object cannot be given less than a push takes it as sent for",
          out.returncode != 0 and "at least 7200 seconds" in out.stderr and bucket.objects() == before)
    out, _ = gc(a, address, env={"RA_JIT_REMOTE_READONLY": "1"})
    check("a machine that only reads collects nothing", out.returncode != 0 and "RA_JIT_REMOTE_READONLY" in out.stderr)
    if not remote.proxy:
        print("given: the checks that need old objects are left out: they need the proxy to say they are old")
        return

    aged(3 * DAYS)
    out, said = gc(a, address, "--dry-run")
    check("--dry-run says what would go and removes nothing", out.returncode == 0
          and said.get("objects that would be removed") == [3] and bucket.objects() == before
          and not remote.proxy.seen("DELETE"), out.stdout.replace("\n", "; ")[:110])
    aged(3 * DAYS, one=False)
    out, said = gc(a, address)
    check("an object written again since it was listed stays", out.returncode == 0 and said.get("objects removed") == [0]
          and bucket.objects() == before)
    aged(3 * DAYS)
    out, said = gc(a, address)
    left = bucket.objects()
    check("old objects that no list names are removed, and only those", out.returncode == 0
          and said.get("objects removed") == [3] and said.get("bytes") == [sum(len(it) for it in strays.values())]
          and set(before) - set(left) == set(strays), out.stdout.replace("\n", "; ")[:110])
    aged(0)
    out = b.ra_jit("cache", "pull", address)
    whole, verified, _ = b.cache("verify")
    check("what the lists name is all still there to pull", out.returncode == 0 and whole and sorted(b.packs()) == sorted(sent))

    # A list this machine does not trust keeps what it names and all its lines.
    key_file = os.path.join(remote.keys, "collector")
    public = a.ra_jit("cache", "keygen", key_file).stdout.strip()
    aged(3 * DAYS)
    out, said = gc(a, address, "--older-than", "1d", env={"RA_JIT_REMOTE_SIGN": key_file, "RA_JIT_REMOTE_TRUST": public})
    check("a list of another signer is left with its lines and what they name",
          out.returncode == 0 and said.get("manifests left that this machine does not trust") == [1]
          and said.get("objects removed") == [0] and bucket.objects() == left, out.stdout.replace("\n", "; ")[:110])

    # A pack that was there before and is listed nowhere is sent again once it is old, and its age starts anew.
    bucket.put(f"packs/{digest(stray)}.pack", stray)
    c = Machine(top, f"g-{remote.name}-c", remote)
    c.add_pack(stray)
    aged(3 * DAYS)
    out = c.ra_jit("cache", "push", address, "another")
    check("a push sends again what has been there unlisted for long", out.returncode == 0
          and len(remote.proxy.seen("PUT", "/packs/")) == 1, out.stdout.strip()[:80])

    out, said = gc(a, address, "--older-than", "1d")
    aged(0)
    check("--older-than takes old packs off the lists and out of the remote", out.returncode == 0
          and said.get("lines taken out") == [4, 0] and said.get("manifests written") == [2]
          and not [it for it in bucket.objects() if it.startswith(("packs/", "keys/"))], out.stdout.replace("\n", "; ")[:110])
    out = a.ra_jit("cache", "push", address)
    d = Machine(top, f"g-{remote.name}-d", remote)
    pulled = d.ra_jit("cache", "pull", address)
    check("what was taken out is pushed again by a machine that has it", out.returncode == 0 and out.stdout.startswith("3 packs")
          and pulled.returncode == 0 and sorted(d.packs()) == sorted(sent), out.stdout.strip()[:80])

    # A store that made one pack of its small ones pushes that one too: the remote has them twice.
    address, bucket = remote.fresh()
    e, f = (Machine(top, f"g-{remote.name}-{it}", remote) for it in "ef")
    for at in range(24):
        e.add_pack(filler(f"{remote.name} small collected {at}", 64 << 10))
    e.ra_jit("cache", "push", address)
    ok, merged, _ = e.cache("gc")
    out = e.ra_jit("cache", "push", address)
    twice = len([it for it in bucket.objects() if it.startswith("packs/")])
    aged(3 * DAYS)
    collect, said = gc(e, address)
    aged(0)
    once = [it for it in bucket.objects() if it.startswith("packs/")]
    pulled = f.ra_jit("cache", "pull", address)
    whole, verified, _ = f.cache("verify")
    check("packs whose keys are all in a merged pack are taken out, and the merged one stays",
          ok and out.returncode == 0 and twice > len(once) and collect.returncode == 0
          and said.get("lines taken out") == [0, twice - len(once)] and said.get("objects removed") == [2 * (twice - len(once))]
          and pulled.returncode == 0 and whole and verified.get("records") == 24 and sorted(f.packs()) == sorted(e.packs()),
          f"{twice} packs, then {len(once)}; " + collect.stdout.splitlines()[0][:70] if collect.stdout else collect.stderr[-80:])


def until(holds, seconds=20):
    """Waits for `holds()` to be true, and says whether it came to be."""
    end = time.time() + seconds
    while time.time() < end:
        if holds():
            return True
        time.sleep(0.1)
    return False


def kept(top, remote, crates, target, workspace):
    """A daemon that stays: it pushes on a schedule and when it ends, and pulls when the manifest changed."""
    print(f"{remote.name}: a daemon that stays", flush=True)
    address, bucket = remote.fresh()
    a, b = (Machine(top, f"d-{remote.name}-{it}", remote) for it in "ab")
    a.checkout(workspace)
    a.cargo(target)
    packs = lambda: [it for it in bucket.objects() if it.startswith("packs/")]
    listed = lambda: len((bucket.get("manifests/default") or b"").splitlines())

    daemon, socket = a.daemon(crates, {"RA_JIT_REMOTE": address, "RA_JIT_REMOTE_PUSH_EVERY": "2s"})
    try:
        check("what the first build compiled is sent without being asked, small as it is",
              until(lambda: listed() >= 1) and len(packs()) == listed(), f"{listed()} packs listed")
        first = listed()
        took = a.edit(socket, "edited once")
        check("what an edit compiled is sent on the schedule", took is not None and until(lambda: listed() > first),
              f"{listed()} packs listed")

        # Another machine adds to the manifest; the daemon has it after its next build lacks something.
        other = b.add_pack(filler(f"{remote.name} from another machine", 256 << 10))
        out = b.ra_jit("cache", "push", address)
        took = a.edit(socket, "edited twice")
        check("a build that lacks something has the daemon pull when the manifest changed",
              out.returncode == 0 and took is not None and until(lambda: other in a.packs())
              and "the manifest of `default` has changed" in a.heard(), a.heard().count("has changed"))
        looks = a.heard().count("has changed")
        took = a.edit(socket, "edited thrice")
        time.sleep(6)
        check("and does not pull when it did not", took is not None and a.heard().count("has changed") == looks)

        if remote.proxy:
            remote.proxy.clear()
            remote.proxy.hold = 3
            took = a.edit(socket, "edited while the remote is slow")
            remote.proxy.hold = 0
            check("no build waits for the remote", took is not None and took < 3, f"{took and round(took, 2)}s with every request held 3s")
    finally:
        tell_to_end(daemon)
        ended = daemon.wait(timeout=90)
    ok, counted, _ = a.cache("stats")
    out = a.ra_jit("cache", "push", address)
    check("told to end, the daemon ends as it was told, with nothing left to send", ended == TOLD_TO_END and ok
          and counted.get("open packs") == 0 and out.returncode == 0 and out.stdout.startswith("0 packs added"),
          f"exit {ended}; {out.stdout.strip()[:40]}")

    daemon, socket = a.daemon(crates, {"RA_JIT_REMOTE": address, "RA_JIT_REMOTE_PUSH_EVERY": "off"})
    try:
        time.sleep(1)
        before = listed()
        took = a.edit(socket, "edited with no schedule")
        time.sleep(4)
        unsent = listed()
    finally:
        tell_to_end(daemon)
        ended = daemon.wait(timeout=90)
    check("with the schedule off an edit is sent when the daemon ends and not before",
          took is not None and unsent == before and ended == TOLD_TO_END and listed() == before + 1,
          f"{before}, {unsent}, then {listed()} packs listed")
    c = Machine(top, f"d-{remote.name}-c", remote)
    c.ws, c.link_args, c.reference = a.ws, a.link_args, a.reference
    log, _ = c.build(crates, env={"RA_JIT_REMOTE": address})
    check("another machine builds the edited program and compiles nothing", compiled(log) == 0 and "edited with no schedule" in c.run(),
          f"{compiled(log)} compiled")


def packs_fetched(remote):
    """How many fetches of packs the proxy saw since it was last asked, or `None` without a proxy."""
    if not remote.proxy:
        return None
    seen = len(remote.proxy.seen("GET", "/packs/"))
    remote.proxy.clear()
    return seen


def over_tls(top, remote):
    """The same service behind a certificate: what ra-jit's TLS trusts and what it does not."""
    if remote.name != "minio" or not shutil.which("openssl"):
        print(f"{remote.name}: TLS is left out: it needs MinIO and openssl", flush=True)
        return
    print(f"{remote.name}: over TLS", flush=True)
    # An authority of the harness's own and a certificate it signs: rustls takes no
    # certificate that vouches for itself.
    certs = os.path.join(top, "certs")
    os.makedirs(certs)
    at = lambda name: os.path.join(certs, name)
    open(at("leaf.ext"), "w").write("subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\n")
    certificate = at("authority.crt")
    for command in (["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-subj", "/CN=ra-jit harness",
                     # Python from 3.13 takes no authority that does not say it may sign.
                     "-addext", "keyUsage=critical,keyCertSign,cRLSign",
                     "-keyout", at("authority.key"), "-out", certificate],
                    ["req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=localhost", "-keyout", at("private.key"),
                     "-out", at("leaf.csr")],
                    ["x509", "-req", "-in", at("leaf.csr"), "-CA", certificate, "-CAkey", at("authority.key"),
                     "-CAcreateserial", "-days", "2", "-extfile", at("leaf.ext"), "-out", at("public.crt")]):
        made = subprocess.run(["openssl", *command], capture_output=True, text=True)
        if made.returncode != 0:
            raise SystemExit(f"openssl {command[0]} failed: {made.stderr}")
    server = Server("minio", remote.server.binary, os.path.join(top, "tls"), certs=certs)
    server.start()
    try:
        endpoint = f"https://localhost:{server.port}"
        bucket = Bucket(endpoint, "ra-jit-tls", "over-tls/", ca=certificate)
        bucket.create()
        address = f"s3://ra-jit-tls/over-tls?endpoint={endpoint}"
        a, b = (Machine(top, f"tls-{it}", remote) for it in "ab")
        name = big_store(a, "over tls")
        trusting = {"SSL_CERT_FILE": certificate}
        sent = a.ra_jit("cache", "push", address, env=trusting)
        out = b.ra_jit("cache", "pull", address, env=trusting)
        check("a big pack sent and fetched over TLS, the certificate trusted through SSL_CERT_FILE",
              sent.returncode == 0 and out.returncode == 0 and b.packs() == {name: True}
              and digest(bucket.get("packs/" + name)) == name[:-5], out.stdout.strip())
        b.empty()
        start = time.time()
        out = b.ra_jit("cache", "pull", address, env={"RA_JIT_REMOTE_TIMEOUT": "2"})
        took = time.time() - start
        check("a certificate nobody vouches for: refused, said, nothing fetched",
              out.returncode != 0 and not b.packs() and not b.partials() and "ertificate" in out.stderr, out.stderr.strip()[-90:])
        check("and it is said at once, not after the waits of a remote that is down", took < 3, f"{took:.2f}s")
    finally:
        server.kill()


def spoiled(top, remote):
    """Objects that are not what their name says."""
    print(f"{remote.name}: objects that are not what was sent", flush=True)
    for what, spoil in (("one byte changed", lambda data: data[:5000] + bytes([data[5000] ^ 1]) + data[5001:]),
                        ("the end cut off", lambda data: data[:len(data) // 2]),
                        ("bytes added", lambda data: data + b"more"),
                        ("made empty", lambda data: b"")):
        address, bucket = remote.fresh()
        a, b = (Machine(top, f"s-{remote.name}-{what.replace(' ', '-')}-{it}", remote) for it in "ab")
        data = filler(f"{remote.name} {what}", MIB)
        name = a.add_pack(data)
        other = big_store(a, f"{remote.name} {what} whole") if what == "one byte changed" else None
        if a.ra_jit("cache", "push", address).returncode != 0:
            raise SystemExit("the push failed")
        sent = bucket.get("packs/" + name)
        bucket.put("packs/" + name, spoil(sent))
        out = b.ra_jit("cache", "pull", address)
        check(f"a pack with {what}: rejected, said, counted, not in the store",
              out.returncode != 0 and name not in b.packs() and not b.partials() and "rejected packs/" + name in out.stderr
              and said_number(out.stderr, "objects rejected") == 1 and (other is None or b.packs() == {other: True}),
              out.stderr.strip().split("\n")[-1][-60:])
        bucket.put("packs/" + name, sent)
        out = b.ra_jit("cache", "pull", address)
        check("put back: fetched", out.returncode == 0 and b.packs().get(name) is True)
    # An object that is what the manifest says and unpacks to something else than the pack.
    address, bucket = remote.fresh()
    a, b = (Machine(top, f"s-{remote.name}-inside-{it}", remote) for it in "ab")
    name = a.add_pack(filler(f"{remote.name} inside", MIB))
    a.ra_jit("cache", "push", address)
    line = bucket.get("manifests/default").decode().split()
    for what, inside in (("another pack inside", packed(filler(f"{remote.name} other", MIB))), ("not compressed inside", b"x" * 4096)):
        bucket.put("packs/" + name + ".zst", inside)
        bucket.put("manifests/default", (" ".join(line[:4] + [digest(inside), str(len(inside))]) + "\n").encode())
        out = b.ra_jit("cache", "pull", address)
        check(f"a compressed object with {what}: rejected after unpacking, nothing left",
              out.returncode != 0 and not b.packs() and not b.partials() and "rejected packs/" + name + ".zst" in out.stderr
              and said_number(out.stderr, "objects rejected") == 1 and b.cache("verify")[0],
              out.stderr.strip().split("\n")[-1][-60:])
    address, bucket = remote.fresh()
    a, b = (Machine(top, f"s-{remote.name}-gone-{it}", remote) for it in "ab")
    name = big_store(a, f"{remote.name} gone")
    a.ra_jit("cache", "push", address)
    bucket.delete("packs/" + name)
    out = b.ra_jit("cache", "pull", address)
    check("a pack the manifest lists that is not there: said", out.returncode != 0 and "is listed and is not there" in out.stderr
          and not b.packs() and not b.partials())


def guarded(top, remote):
    """Lists nobody signed, lists that were put back, and objects longer than they are listed as."""
    print(f"{remote.name}: unsigned lists, old lists, long objects", flush=True)
    nothing_asked = {"RA_JIT_REMOTE_UNSIGNED": ""}
    address, bucket = remote.fresh()
    a, b, c, d = (Machine(top, f"u-{remote.name}-{it}", remote) for it in "abcd")
    first = a.add_pack(filler(f"{remote.name} guarded 1", MIB))
    out = a.ra_jit("cache", "push", address, env=nothing_asked)
    check("a machine with no keys pushes without being asked anything", out.returncode == 0 and "packs/" + first in bucket.objects())
    for value in ("", "0", "true"):
        out = b.ra_jit("cache", "pull", address, env={"RA_JIT_REMOTE_UNSIGNED": value})
        check(f"a list nobody signed is not taken with RA_JIT_REMOTE_UNSIGNED={value!r}",
              out.returncode != 0 and "RA_JIT_REMOTE_UNSIGNED=1" in out.stderr and not b.packs() and not b.partials(),
              out.stderr.strip()[-60:])
    out = b.ra_jit("cache", "pull", address)
    check("and is with RA_JIT_REMOTE_UNSIGNED=1", out.returncode == 0 and b.packs().get(first) is True, out.stdout.strip())

    # A pack is as long as its list says, as it is sent and as it unpacks: one byte less
    # listed of either, and it is not what was listed.
    # The same of one an older ra-jit sent compressed, listed with what was sent.
    manifest = bucket.get("manifests/default").decode()
    pack, size, keys, fmt = manifest.split()
    zipped = packed(bucket.get(f"packs/{pack}.pack"))
    bucket.put(f"packs/{pack}.pack.zst", zipped)
    zdigest, zsize = digest(zipped), len(zipped)
    b.empty()
    for what, line in [(f"listed as {listed}", f"{pack} {listed} {keys} {fmt}")
                       for listed in (0, 1, int(size) - 1)] + \
                      [(f"{zsize} sent, listed as {listed} unpacked", f"{pack} {listed} {keys} {fmt} {zdigest} {zsize}")
                       for listed in (0, 1, int(size) - 1)] + \
                      [(f"{zsize} sent, listed as {listed} sent", f"{pack} {size} {keys} {fmt} {zdigest} {listed}")
                       for listed in (0, zsize - 1)]:
        bucket.put("manifests/default", (line + "\n").encode())
        out = b.ra_jit("cache", "pull", address)
        check(f"a pack of {size} bytes, {what}: rejected, nothing kept",
              out.returncode != 0 and "1 of them rejected" in out.stderr and not b.packs() and not b.partials()
              and not [it for it in os.listdir(b.store) if it not in ("version", "lock", "turns")],
              out.stderr.strip().splitlines()[0][-60:])
    # A pack that is small as sent compressed and unpacks to a great deal, listed as small:
    # stopped there.
    bombs, bomb_bucket = remote.fresh()
    bomber = Machine(top, f"u-{remote.name}-bomb", remote)
    bomb = bomber.add_pack(pack_of([(hashlib.sha256(b"bomb").digest(), bytes(64 * MIB))]))
    bomber.ra_jit("cache", "push", bombs)
    pack, size, keys, fmt, *sent_as = bomb_bucket.get("manifests/default").decode().split()
    check("a pack that compresses to a quarter is sent compressed",
          len(sent_as) == 2 and int(sent_as[1]) * 4 <= int(size), " ".join(sent_as))
    zipped = bomb_bucket.get(f"packs/{pack}.pack.zst") if sent_as else packed(bomb_bucket.get(f"packs/{pack}.pack"))
    bomb_bucket.put(f"packs/{pack}.pack.zst", zipped)
    bomb_bucket.put("manifests/default", f"{pack} {MIB} {keys} {fmt} {digest(zipped)} {len(zipped)}\n".encode())
    address, sent = bombs, len(zipped)
    out = b.ra_jit("cache", "pull", address)
    most = max([os.path.getsize(os.path.join(b.store, it)) for it in os.listdir(b.store)
                if os.path.isfile(os.path.join(b.store, it))] + [0])
    check(f"{sent} bytes that unpack to 64 MiB, listed as 1 MiB: rejected, nothing kept",
          out.returncode != 0 and "1 of them rejected" in out.stderr and sent < MIB and not b.packs() and not b.partials()
          and most < MIB,
          out.stderr.strip().splitlines()[0][-60:])
    # A list is read to 16 MiB and no further.
    address, bucket = remote.fresh()
    a.ra_jit("cache", "push", address, env=nothing_asked)
    manifest = bucket.get("manifests/default").decode()
    bucket.put("manifests/default", manifest.encode() + b"\n" * (16 * MIB - len(manifest)))
    out = b.ra_jit("cache", "pull", address)
    check("a manifest of 16 MiB is read", out.returncode == 0 and b.packs().get(first) is True, out.stdout.strip()[:50])
    b.empty()
    bucket.put("manifests/default", manifest.encode() + b"\n" * (16 * MIB - len(manifest) + 1))
    out = b.ra_jit("cache", "pull", address)
    check("one byte more and it is not", out.returncode != 0 and "is longer than" in out.stderr and not b.packs(),
          out.stderr.strip().splitlines()[0][-70:])

    # Signed lists have a turn, and a machine takes none older than it took before.
    key_file = os.path.join(remote.keys, "guarded.key")
    public = a.ra_jit("cache", "keygen", key_file).stdout.strip()
    address, bucket = remote.fresh()
    signing, trusting = {"RA_JIT_REMOTE_SIGN": key_file}, {"RA_JIT_REMOTE_TRUST": public, "RA_JIT_REMOTE_UNSIGNED": ""}
    turn = lambda listed: int(listed.decode().splitlines()[-1].split()[2])
    a.ra_jit("cache", "push", address, env=signing)
    old = bucket.get("manifests/default")
    out = c.ra_jit("cache", "pull", address, env=trusting)
    check("C, trusting A's key, takes A's first list", out.returncode == 0 and set(c.packs()) == {first}, out.stdout.strip()[:40])
    second = a.add_pack(filler(f"{remote.name} guarded 2", MIB))
    a.ra_jit("cache", "push", address, env=signing)
    new = bucket.get("manifests/default")
    out = c.ra_jit("cache", "pull", address, env=trusting)
    check("and the second, which has a later turn", out.returncode == 0 and set(c.packs()) == {first, second}
          and turn(new) > turn(old) >= int(time.time()) - 3600, f"{turn(old)} then {turn(new)}")
    bucket.put("manifests/default", old)
    # Its packs go and what it remembers of the lists stays.
    for name in c.packs():
        os.remove(os.path.join(c.store, name))
    out = c.ra_jit("cache", "pull", address, env=trusting)
    check("the first list put back: refused as older, nothing fetched",
          out.returncode != 0 and "an older list is not taken" in out.stderr and not c.packs() and not c.partials(),
          out.stderr.strip().splitlines()[0][-70:])
    out = d.ra_jit("cache", "pull", address, env=trusting)
    check("a machine that never pulled takes it: it has nothing to tell by", out.returncode == 0 and set(d.packs()) == {first})
    out = c.ra_jit("cache", "forget", address)
    again = c.ra_jit("cache", "pull", address, env=trusting)
    check("`cache forget` has C take whatever is there", out.returncode == 0 and "forgotten" in out.stdout
          and again.returncode == 0 and set(c.packs()) == {first}, out.stdout.strip()[-40:])
    # The signer, under which the list was put back, signs nothing on top of it.
    third = a.add_pack(filler(f"{remote.name} guarded 3", MIB))
    out = a.ra_jit("cache", "push", address, env=signing)
    newest = bucket.get("manifests/default")
    names = {line.split()[0] + ".pack" for line in newest.decode().splitlines()[:-1]}
    check("A, pushing over the list that was put back, writes a new one with a later turn",
          out.returncode == 0 and "an older list is not taken" in out.stderr and turn(newest) > turn(new) and third in names,
          f"turn {turn(newest)}, {len(names)} packs")
    bucket.put("manifests/default", newest)
    out = c.ra_jit("cache", "pull", address, env=trusting)
    check("and C, given the newest, takes it and then not the second again", out.returncode == 0 and third in c.packs())
    bucket.put("manifests/default", new)
    out = c.ra_jit("cache", "pull", address, env=trusting)
    check("the second list put back after the third: refused", out.returncode != 0 and "an older list is not taken" in out.stderr)

    # Collecting the remote is not a way around the signature: a machine with no key takes
    # no line out of a signed list, and removes nothing the list names.
    bucket.put("manifests/default", newest)
    before = bucket.objects()
    for env in (nothing_asked, {}):
        out = d.ra_jit("cache", "gc", "--remote", address, "--older-than", "0s", "--unlisted-after", "2h", env=env)
        check(f"`gc --remote` by a machine with no key{' told to take unsigned lists' if not env else ''}: the signed list stays",
              bucket.get("manifests/default") == newest and bucket.objects() == before, (out.stdout + out.stderr).strip()[-70:])
    d.add_pack(filler(f"{remote.name} guarded by d", MIB))
    out = d.ra_jit("cache", "push", address, env=nothing_asked)
    check("nor does its push write over the signed list", out.returncode != 0 and "is signed" in out.stderr
          and bucket.get("manifests/default") == newest, out.stderr.strip()[-70:])
    time.sleep(1.1)
    out = a.ra_jit("cache", "gc", "--remote", address, "--older-than", "0s", "--unlisted-after", "2h", env=signing)
    after = bucket.get("manifests/default")
    check("the signer collects: the list is signed again, at a later turn",
          out.returncode == 0 and after != newest and after.decode().splitlines()[-1].startswith(f"signed {public} ")
          and turn(after) > turn(newest), f"turn {turn(after)}")
    out = c.ra_jit("cache", "pull", address, env=trusting)
    check("and C takes it", out.returncode == 0, out.stdout.strip()[:50])


def poisoned(top, remote, a, crates, target, workspace, address, bucket, expected):
    """A remote that holds other code under the keys of A's."""
    print(f"{remote.name}: a remote that lies", flush=True)
    objects = bucket.objects()
    manifest = bucket.get("manifests/default").decode()
    listed = [line.split() for line in manifest.splitlines() if not line.startswith("signed")]

    def machine(name):
        m = Machine(top, f"p-{remote.name}-{name}", remote)
        m.checkout(workspace)
        m.cargo(target)
        return m

    # 1. Every pack A sent, changed in place: no name fits. A pack that compresses to a quarter
    #    less is sent as `.pack.zst` and listed with what was sent (six fields). Which packs
    #    those are, and the order the manifest lists them in, change from build to build, and
    #    one pack can be enough for B to compile almost nothing: so all of them are changed.
    def change(fields):
        compressed = len(fields) == 6
        name = "packs/" + fields[0] + ".pack" + (".zst" if compressed else "")
        data = bucket.get(name)
        records = records_of(unpacked(data) if compressed else data)
        changed = pack_of([(key, value[:-1] + bytes([value[-1] ^ 0x55]) if value else value) for key, value in records])
        bucket.put(name, packed(changed) if compressed else changed)
        return name, data, records, changed

    sent = [change(fields) for fields in listed]
    _, _, records, changed = sent[0]
    b = machine("changed")
    log, _ = b.build(crates, env={"RA_JIT_REMOTE": address})
    check("code changed under A's name: rejected, B compiles its own and prints the same",
          all("rejected " + name in log for name, *_ in sent) and compiled(log) > 0 and b.run() == expected
          and not any(fields[0] + ".pack" in b.packs() for fields in listed),
          f"{len(sent)} packs, {compiled(log)} compiled")
    for name, data, *_ in sent:
        bucket.put(name, data)

    # 2. The same with the manifest rewritten to name the changed pack by its own hash: every
    #    hash fits. Only a signature tells this list from A's.
    key_file = os.path.join(remote.keys, "a.key")
    public = a.ra_jit("cache", "keygen", key_file).stdout.strip()
    signed_address, signed_bucket = remote.fresh()
    signing = {"RA_JIT_REMOTE_SIGN": key_file}
    out = a.ra_jit("cache", "push", signed_address, env=signing)
    trusted = signed_bucket.get("manifests/default").decode()
    check("A signs its manifest", out.returncode == 0 and trusted.splitlines()[-1].startswith(f"signed {public} "))
    b = machine("signed")
    log, _ = b.build(crates, env={"RA_JIT_REMOTE": signed_address, "RA_JIT_REMOTE_TRUST": public})
    check("B, trusting A's key, fetches and compiles nothing", compiled(log) == 0 and b.run() == expected)

    forged_name = digest(changed)
    keys = b"".join(key for key, _ in records)
    forged = "".join(f"{forged_name} {len(changed)} {digest(keys)} {FORMAT}\n" if fields[0] == listed[0][0] else " ".join(fields) + "\n"
                     for fields in listed)
    def forge(to, last_line=""):
        to.put("packs/" + forged_name + ".pack", changed)
        to.put("keys/" + digest(keys), keys)
        to.put("manifests/default", (forged + last_line).encode())

    forge(signed_bucket, trusted.splitlines()[-1] + "\n")
    b = machine("forged")
    log, _ = b.build(crates, env={"RA_JIT_REMOTE": signed_address, "RA_JIT_REMOTE_TRUST": public})
    check("a forged manifest under A's old signature: refused, nothing fetched, same program",
          "is not what" in log and not b.packs().get(forged_name + ".pack") and compiled(log) > 0 and b.run() == expected,
          f"{compiled(log)} compiled")
    forge(signed_bucket)
    b = machine("unsigned")
    log, _ = b.build(crates, env={"RA_JIT_REMOTE": signed_address, "RA_JIT_REMOTE_TRUST": public})
    check("a forged manifest with no signature: refused", "is not signed" in log and forged_name + ".pack" not in b.packs()
          and b.run() == expected)
    other_key = os.path.join(remote.keys, "other.key")
    a.ra_jit("cache", "keygen", other_key)
    c = Machine(top, f"p-{remote.name}-other", remote)
    shutil.copytree(a.store, c.store)
    out = c.ra_jit("cache", "push", signed_address, env={"RA_JIT_REMOTE_SIGN": other_key})
    b = machine("stranger")
    log, _ = b.build(crates, env={"RA_JIT_REMOTE": signed_address, "RA_JIT_REMOTE_TRUST": public})
    check("a manifest signed by a key B does not trust: refused", out.returncode == 0 and "not a key this machine trusts" in log
          and compiled(log) > 0 and b.run() == expected)
    out = c.ra_jit("cache", "push", signed_address, env={"RA_JIT_REMOTE_TRUST": public})
    check("a machine that trusts and cannot sign does not push", out.returncode != 0 and "RA_JIT_REMOTE_SIGN" in out.stderr)

    # 3. What B does with the forged manifest when it trusts nobody: said in the report.
    forge(bucket)
    b = machine("trusting")
    try:
        log, _ = b.build(crates, env={"RA_JIT_REMOTE": address})
        took = forged_name + ".pack" in b.packs()
        same = b.run() == expected
        print(f"  (with RA_JIT_REMOTE_UNSIGNED=1 and no RA_JIT_REMOTE_TRUST the forged pack is {'taken' if took else 'not taken'}; "
              f"the program prints {'the same' if same else 'something else'})")
    except (SystemExit, subprocess.TimeoutExpired) as error:
        print(f"  (with RA_JIT_REMOTE_UNSIGNED=1 and no RA_JIT_REMOTE_TRUST the forged pack is taken and the build fails: {str(error)[-200:]!r})")
    bucket.put("manifests/default", manifest.encode())


def lazy(top, remote, a, crates, target, workspace, expected):
    """Packs fetched when a build asks for a key in them."""
    print(f"{remote.name}: packs fetched as the build asks", flush=True)
    address, bucket = remote.fresh()
    unused = big_store(a, f"{remote.name} unused")
    out = a.ra_jit("cache", "push", address)
    os.remove(os.path.join(a.store, unused))
    b = Machine(top, f"lazy-{remote.name}", remote)
    b.checkout(workspace)
    b.cargo(target)
    log, _ = b.build(crates, env={"RA_JIT_REMOTE": address, "RA_JIT_REMOTE_LAZY": "1"})
    check("B compiles nothing, and prints the same", out.returncode == 0 and compiled(log) == 0 and b.run() == expected,
          f"{compiled(log)} compiled")
    check("the pack no key was asked of is not fetched", b.packs() and unused not in b.packs() and all(b.packs().values()),
          f"{len(b.packs())} of {len(bucket.objects()) // 2} packs")


def checks():
    top = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-remote-"))
    workspace, crates, target = os.path.join(HERE, "stress"), ["sapp", "sbase", "smid"], ["-p", "sapp"]
    remotes = []
    if REAL:
        remotes.append(Remote("given", None, top))
    else:
        for kind, variable in (("minio", "RA_JIT_MINIO"), ("weed", "RA_JIT_WEED")):
            binary = find(kind, variable)
            if binary:
                remotes.append(Remote(kind, Server(kind, binary, top), top))
            else:
                print(f"{kind}: not found ({variable} names it), left out")
    if not remotes:
        raise SystemExit("no S3 service to run against")
    try:
        for remote in remotes:
            remote.start()
            a, b, address, bucket, expected = round_trip(top, remote, remote.name, crates, target, workspace)
            configuration(top, remote, a, address, bucket)
            if remote.server:
                over_tls(top, remote)
            stored(top, remote)
            guarded(top, remote)
            collected(top, remote)
            kept(top, remote, crates, target, workspace)
            spoiled(top, remote)
            poisoned(top, remote, a, crates, target, workspace, address, bucket, expected)
            lazy(top, remote, a, crates, target, workspace, expected)
            if remote.server:
                transfers(top, remote)
            else:
                print("given: the transfers that break are left out: they need the proxy and a service to kill")
    finally:
        for remote in remotes:
            if remote.server:
                remote.server.kill()
    print(f"{right}/{total} right")
    if right == total:
        shutil.rmtree(top, ignore_errors=True)
    else:
        print(f"kept: {top}")
    sys.exit(0 if right == total else 1)


def measure(args):
    """A fresh machine to a running program: cargo and ra-jit alone, then with what A sent."""
    top = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-remote-measure-"))
    crates, target = args.crate.split(","), ["-p", args.package, "--bin", args.bin]
    binary = find("minio", "RA_JIT_MINIO")
    remote = Remote("given", None, top) if REAL else Remote("minio", Server("minio", binary, top), top)
    run_args = shlex.split(args.run)

    def machine(name):
        m = Machine(top, name, remote)
        m.checkout(args.workspace)
        return m

    def fresh(name, env, deps):
        """Everything a machine with a checkout and a toolchain does to have the program run."""
        m = machine(name)
        if remote.proxy:
            remote.proxy.clear()
        start = time.time()
        pulled = ""
        if deps:
            directory = os.path.join("target", "debug")
            out = m.ra_jit("deps", "pull", address, directory, cwd=m.ws)
            pulled = out.stdout.strip() + " | " + out.stderr.strip().split("\n")[-1]
        t_deps = time.time() - start
        t_cargo = m.cargo(target)
        log, t_ra_jit = m.build(crates, env=env)
        printed = m.run(args=run_args)
        whole = time.time() - start
        lines = [line for line in log.splitlines() if line.startswith(("remote:", "store:", "cold:"))]
        requests = (len(remote.proxy.requests), remote.proxy.received, remote.proxy.sent) if remote.proxy else None
        if deps is not None:
            shutil.rmtree(m.dir, ignore_errors=True)
        return m, {"whole": whole, "deps": t_deps, "cargo": t_cargo, "ra-jit": t_ra_jit, "lines": lines, "pulled": pulled,
                   "proxy": requests, "printed": printed, "compiled": compiled(log)}

    def show(what, got):
        print(f"{what}: {got['whole']:.1f}s to a running program "
              f"(deps pull {got['deps']:.1f}s, cargo {got['cargo']:.1f}s, ra-jit {got['ra-jit']:.1f}s), {got['compiled']} compiled")
        if got["pulled"]:
            print(f"    {got['pulled']}")
        for line in got["lines"]:
            print(f"    {line}")
        if got["proxy"]:
            print("    the proxy saw %d requests, %d bytes down, %d bytes up" % got["proxy"])

    try:
        remote.start()
        address, bucket = remote.fresh()
        a, alone = fresh("a", {"RA_JIT_REMOTE": address}, None)
        show("A, nothing anywhere, sends its store", alone)
        out = a.ra_jit("deps", "push", address, os.path.join("target", "debug"), cwd=a.ws)
        print(f"    {out.stdout.strip()} | {out.stderr.strip().splitlines()[-1]}")
        shutil.rmtree(a.dir, ignore_errors=True)
        sizes = bucket.objects()
        print(f"    the bucket: {len(sizes)} objects, {sum(sizes.values())} bytes")
        _, without = fresh("without", {}, False)
        show("B without the remote", without)
        _, with_code = fresh("code", {"RA_JIT_REMOTE": address, "RA_JIT_REMOTE_READONLY": "1"}, False)
        show("B with the store of the remote only", with_code)
        _, with_all = fresh("all", {"RA_JIT_REMOTE": address, "RA_JIT_REMOTE_READONLY": "1"}, True)
        show("B with the store and cargo's units", with_all)
        _, lazily = fresh("lazy", {"RA_JIT_REMOTE": address, "RA_JIT_REMOTE_READONLY": "1", "RA_JIT_REMOTE_LAZY": "1"}, True)
        show("B the same, packs fetched as asked for", lazily)
        same = len({it["printed"] for it in (alone, without, with_code, with_all, lazily)}) == 1
        print("every program printed the same" if same else "THE PROGRAMS DO NOT PRINT THE SAME")
        sys.exit(0 if same else 1)
    finally:
        if remote.server:
            remote.server.kill()
        if not args.keep:
            shutil.rmtree(top, ignore_errors=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--measure", dest="workspace")
    parser.add_argument("-p", dest="package")
    parser.add_argument("--bin")
    parser.add_argument("--crate")
    parser.add_argument("--run", default="--version")
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    if args.workspace:
        measure(args)
    else:
        checks()
