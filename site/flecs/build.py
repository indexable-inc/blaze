"""Build flecs-doc.wasm: flecs-doc.c and Flecs's amalgamated source, for wasm32-wasi.

    python3 site/flecs/build.py <flecs checkout>

The wasm is checked in, so building the site needs only bun. Rebuild it after changing flecs-doc.c
or moving to another Flecs; FLECS_VERSION records the Flecs it was built from. Needs zig on PATH.
"""

import asyncio
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

# Only the addons a script needs: the parser, reflection for script-defined structs, JSON out,
# prefabs, and logging so a script error says where it is.
ADDONS = ["FLECS_SCRIPT", "FLECS_META", "FLECS_JSON", "FLECS_PREFAB", "FLECS_LOG"]


async def run(*cmd: str, cwd: Path | None = None) -> str:
    proc = await asyncio.create_subprocess_exec(
        *cmd, cwd=cwd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
    )
    async with asyncio.timeout(600):
        out, err = await proc.communicate()
    if proc.returncode != 0:
        tail = err.decode(errors="replace")[-4000:]
        raise SystemExit(f"{cmd[0]} exited {proc.returncode}: {' '.join(cmd)}\n{tail}")
    return out.decode().strip()


async def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    flecs = Path(sys.argv[1]).resolve()
    distr = flecs / "distr"
    if not (distr / "flecs.c").is_file():
        raise SystemExit(f"{distr}/flecs.c not found: pass a Flecs checkout")
    async with asyncio.TaskGroup() as tg:
        version = tg.create_task(run("git", "describe", "--tags", "--always", cwd=flecs))
        tg.create_task(
            run(
                "zig", "cc", "-target", "wasm32-wasi", "-Os", "-w",
                # Flecs knows no wasi target; COSMOCC selects its plain POSIX path (no execinfo).
                "-D__COSMOCC__", "-DFLECS_CUSTOM_BUILD", *(f"-D{a}" for a in ADDONS),
                f"-I{distr}", "-o", str(HERE / "flecs-doc.wasm"),
                str(HERE / "flecs-doc.c"), str(distr / "flecs.c"),
            )
        )
    (HERE / "FLECS_VERSION").write_text(version.result() + "\n")
    size = (HERE / "flecs-doc.wasm").stat().st_size
    print(f"flecs-doc.wasm: {size} bytes, flecs {version.result()}")


asyncio.run(main())
