#!/usr/bin/env python3
"""Every function blaze refuses on the bench apps, counted by reason (lane norustc-refused).

    inventory.py OUT NAME...     NAME: a bench.py project, or `fixture` (harness/norustc/fixture)

Each project is copied from cargo_ra_jit.fetch, then `cargo blaze build` (bench.py's `build`)
and `cargo blaze test --no-run` (its `test`) run with a home of their own; RA_JIT_SKIPPED
(appended to by the patched ra-jit) gets every refusal of every build. The standard library's
unit is made once, with its refusals in OUT/std.skipped. Projects run two at a time.
"""
import asyncio, json, os, shutil, sys, time

HARNESS = os.environ["NRR_HARNESS"]
sys.path.insert(0, HARNESS)
import bench as benchpy  # noqa: E402
import cargo_ra_jit  # noqa: E402
from coldbuild import env_for  # noqa: E402

STEP_TIMEOUT = 5400


async def run(step, argv, cwd, env, log):
    with open(log, "ab") as out:
        out.write(f"\n$ {' '.join(argv)}  (cwd {cwd})\n".encode())
        out.flush()
        start = time.monotonic()
        proc = await asyncio.create_subprocess_exec(*argv, cwd=cwd, env=env, stdout=out, stderr=out)
        try:
            async with asyncio.timeout(STEP_TIMEOUT):
                rc = await proc.wait()
        except TimeoutError:
            proc.kill()
            rc = "timeout"
        took = time.monotonic() - start
        out.write(f"[{step}] rc={rc} took={took:.1f}s\n".encode())
    return {"step": step, "rc": rc, "took": round(took, 1)}


async def project(out, name, remote, limit):
    async with limit:
        logs = os.path.join(out, name)
        os.makedirs(logs, exist_ok=True)
        log = os.path.join(logs, "steps.log")
        if name == "fixture":
            spec = {"build": [], "test": [], "env": {}}
            src = os.path.join(HARNESS, "norustc", "fixture")
        else:
            spec = benchpy.PROJECTS[name]
            src = cargo_ra_jit.fetch(name, spec)
        ws = os.path.join(out, "ws", name)
        shutil.rmtree(ws, ignore_errors=True)
        shutil.copytree(src, ws, symlinks=True, ignore=lambda at, names: [n for n in names if n in ("target", ".git")])
        env = dict(env_for(os.path.join(out, "homes", name), remote), **spec.get("env", {}))
        env["RA_JIT_SKIPPED"] = os.path.join(logs, "skipped")
        steps = [await run("fetch", ["cargo", "fetch"], ws, env, log)]
        steps.append(await run("build", ["cargo", "blaze", "build", *spec.get("build", [])], ws, env, log))
        if spec.get("test") is not None:
            steps.append(await run("test", ["cargo", "blaze", "test", "--no-run", *spec["test"]], ws, env, log))
        steps.append(await run("stop", ["cargo", "ra-jit", "stop"], ws, env, log))
        json.dump(steps, open(os.path.join(logs, "steps.json"), "w"), indent=1)
        print(name, steps, flush=True)


async def main():
    out, names = sys.argv[1], sys.argv[2:]
    os.makedirs(out, exist_ok=True)
    os.environ["RA_JIT_SKIPPED"] = os.path.join(out, "std.skipped")
    remote, made = benchpy.shipped_std(cargo_ra_jit.RA_JIT, "cranelift", out)
    os.environ.pop("RA_JIT_SKIPPED")
    print(f"std made in {made:.1f}s, remote {remote}", flush=True)
    limit = asyncio.Semaphore(int(os.environ.get("NRR_PARALLEL", "2")))
    async with asyncio.TaskGroup() as group:
        for name in names:
            group.create_task(project(out, name, remote, limit))


if __name__ == "__main__":
    asyncio.run(main())
